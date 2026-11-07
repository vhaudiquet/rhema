//! The HTTP surface: the resident agent's chat routes, its session
//! inventory, its watch CRUD, its setup badge (`/v1/bootstrap`), the
//! external-event ingress (`/v1/ingest`), the notification queue the
//! devices poll, and the health probe that stays open when everything
//! else is tokened.
//!
//! The shape of a chat request: ingest the message, then drain the
//! interactive dispatcher — run passes until one admits nothing and the
//! key has nothing waiting — so queued follow-ups drain before the HTTP
//! answer returns. The reply itself arrives on the event stream, not in
//! the POST answer: the POST only says the message was accepted. That
//! stream answers with the key's recorded turns first and the live feed
//! after, cut atomically in the hub — a (re)connecting client resets its
//! transcript and lets the replay rebuild it, so a reload is not a blank
//! page and a dropped stream is not a gap. (The OpenAI shim is the one
//! caller that still subscribes before ingesting: it consumes the reply
//! off its own receiver.)
//!
//! The drain loop is capped. A pass that runs a foreign key's turn is
//! still a pass; a pathological backlog stops at
//! [`MAX_DRAIN_PASSES`] and leaves the rest to the daemon's tick, which
//! drains the interactive dispatcher as its backstop.
//!
//! A chat message may carry images. The route checks the budget —
//! [`MAX_INBOUND_IMAGE_BYTES`] decoded per image, [`MAX_INBOUND_IMAGES`]
//! a message — and the dispatch decodes the bodies into the attachment
//! store: the stream replays ids and media types, never megabytes, and
//! a client fetches the bytes through the attachments route it already
//! has.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use rhema_autonomous_agent::Driver;
use rhema_claw_tools_contract::Auth;
use rhema_claw_tools_contract::CONTRACT_HEADER;
use rhema_claw_tools_contract::CONTRACT_VERSION;
use rhema_core::ImageMediaType;
use rhema_rsd::{
    DispatchError, Dispatcher, IngestResult, KeyRecord, KeyState, ResourceKey, SqliteStore, Store,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::ReceiverStream;

use crate::backends::BackendRegistry;
use crate::bootstrap::Bootstrap;
use crate::hub::{EventHub, TurnEvent};
use crate::interactive::WireImage;
use crate::notifications::NotificationStore;
use crate::steer::SteerRegistry;
use crate::task_store::TaskStore;
use crate::watch_store::{ClawStore, validate_watch};

/// How many `run_once` passes one POST may spend draining.
pub(crate) const MAX_DRAIN_PASSES: usize = 10;

/// How often the SSE keep-alive comment goes out.
pub(crate) const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Everything the routes touch, shared behind `Arc`s. Built by the
/// daemon's assembly; [`ClawState::new`] is its only constructor.
#[derive(Clone)]
pub struct ClawState {
    dispatcher: Arc<Dispatcher>,
    driver: Arc<Driver>,
    store: Arc<SqliteStore>,
    watches: Arc<ClawStore>,
    /// The calendar table: `/v1/calendars` writes it, the reminders
    /// source reads it fresh every pass.
    calendars: Arc<rhema_calendar_integration::CalendarStore>,
    notifications: Arc<NotificationStore>,
    /// The scheduled-task table the `/v1/tasks` routes serve — the same
    /// rows the resident scheduler fires.
    tasks: Arc<TaskStore>,
    bootstrap: Arc<Bootstrap>,
    hub: Arc<EventHub>,
    /// The steer registry the messages route signals after its ingest:
    /// a chat message into a key whose turn is live cuts that turn
    /// short (see [`crate::steer`]).
    steer: Arc<SteerRegistry>,
    backends: Arc<BackendRegistry>,
    settings: Arc<crate::settings::SettingsStore>,
    identity: Arc<crate::identity::IdentityStore>,
    /// The accounts layer the identity routes serve. Like every consumer
    /// of the store, the routes only ever see redacted records.
    accounts: Arc<dyn rhema_identity::IdentityStore>,
    /// The consent table the consent routes read and answer into — the
    /// same rows the interactive handler's `ask_secret` tool waits on.
    consents: Arc<crate::consents::ConsentStore>,
    /// The wake bridge between those answers and the waiting tools.
    bridge: Arc<crate::consents::WaitBridge>,
    /// The attachment store the `attach_file` tool copies into and the
    /// fetch route serves from.
    attachments: Arc<crate::attachments::AttachmentStore>,
    /// The phone tables: the command queue the device polls and the
    /// reported state the ingest door feeds.
    phone: Arc<crate::phone::PhoneStore>,
    token: Option<String>,
    /// The scoped agent token the client tools hold: a second bearer
    /// answering only the contract's data routes (see
    /// [`require_auth`]).
    agent_token: Option<String>,
    /// The ingest credential integrations hold: a third bearer
    /// answering the event door only (see [`require_auth`]).
    ingest_token: Option<String>,
    /// The generic sources `/v1/ingest` has registered on the
    /// dispatcher, so a source name registers exactly once however many
    /// events race in through it.
    generic_sources: Arc<Mutex<BTreeSet<String>>>,
}

impl ClawState {
    /// The state over the interactive dispatcher and its driver, the
    /// shared dispatch store, the watch table, the notification and
    /// task tables, the bootstrap store, the event hub, the account
    /// store, the consent table, the attachment store, and the
    /// configured bearers — the human `token` and the scoped
    /// `agent_token`.
    #[expect(
        clippy::too_many_arguments,
        reason = "eighteen parts, each load-bearing"
    )]
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        driver: Arc<Driver>,
        store: Arc<SqliteStore>,
        watches: Arc<ClawStore>,
        calendars: Arc<rhema_calendar_integration::CalendarStore>,
        notifications: Arc<NotificationStore>,
        tasks: Arc<TaskStore>,
        bootstrap: Arc<Bootstrap>,
        hub: Arc<EventHub>,
        steer: Arc<SteerRegistry>,
        backends: Arc<BackendRegistry>,
        settings: Arc<crate::settings::SettingsStore>,
        identity: Arc<crate::identity::IdentityStore>,
        accounts: Arc<dyn rhema_identity::IdentityStore>,
        consents: Arc<crate::consents::ConsentStore>,
        bridge: Arc<crate::consents::WaitBridge>,
        attachments: Arc<crate::attachments::AttachmentStore>,
        phone: Arc<crate::phone::PhoneStore>,
        token: Option<String>,
        agent_token: Option<String>,
        ingest_token: Option<String>,
    ) -> Self {
        Self {
            dispatcher,
            driver,
            store,
            watches,
            calendars,
            notifications,
            tasks,
            bootstrap,
            hub,
            steer,
            backends,
            settings,
            identity,
            accounts,
            consents,
            bridge,
            attachments,
            phone,
            token,
            agent_token,
            ingest_token,
            generic_sources: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Every dispatch key with a record — interactive and forge alike,
    /// both dispatchers share one store — with its status.
    pub fn sessions(&self) -> Result<Vec<Value>, DispatchError> {
        let mut sessions = Vec::new();
        for key in self.store.keys()? {
            let status = self.dispatcher.status(&key)?;
            sessions.push(json!({
                "key": status.key.as_str(),
                "state": status.state,
                "revision": status.revision,
                "inbox_depth": status.inbox_depth,
                "last_outcome": status.last_outcome,
            }));
        }
        Ok(sessions)
    }

    /// The interactive dispatcher: message ingest and key status.
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// The interactive driver: the drain loops run it.
    pub fn driver(&self) -> &Driver {
        &self.driver
    }

    /// The event hub: turns are streamed from it.
    pub fn hub(&self) -> &EventHub {
        &self.hub
    }

    /// The steer registry: the messages route signals it.
    pub fn steer(&self) -> &SteerRegistry {
        &self.steer
    }

    /// The configured model — the shim's echo default.
    /// The backend registry: the provider list and the active pair.
    pub fn backends(&self) -> &BackendRegistry {
        &self.backends
    }

    /// The settings store: where a runtime switch persists.
    pub fn settings(&self) -> &crate::settings::SettingsStore {
        &self.settings
    }

    /// The configured bearer token, when the surface is guarded.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The agent's presentation identity.
    pub fn identity(&self) -> &crate::identity::IdentityStore {
        &self.identity
    }

    /// The accounts layer: the identity routes list, save, and forget
    /// through it, redacted records only.
    pub fn accounts(&self) -> &dyn rhema_identity::IdentityStore {
        self.accounts.as_ref()
    }

    /// The watch table: a saved account's forge watch registers here.
    pub fn watches(&self) -> &ClawStore {
        &self.watches
    }
}

/// The app router: `/healthz` open, `/v1` behind the bearer check when
/// the config set a token.
pub fn router(state: ClawState) -> Router {
    let v1 = Router::new()
        .route("/sessions", get(sessions).post(sessions_create))
        .route(
            "/sessions/{key}/messages",
            post(messages).layer(axum::extract::DefaultBodyLimit::max(MAX_MESSAGE_BODY_BYTES)),
        )
        .route("/sessions/{key}/events", get(events))
        .route("/sessions/{key}/history", get(session_history))
        .route("/bootstrap", get(bootstrap_status))
        .route("/watches", get(watches_list).post(watches_add))
        .route("/watches/{forge}/{*repo}", delete(watches_remove))
        .route("/calendars", get(calendars_list).post(calendars_add))
        .route("/calendars/{name}", delete(calendars_remove))
        .route("/accounts", get(accounts_list).post(accounts_add))
        .route("/accounts/{id}", delete(accounts_remove))
        .route("/consents", get(consents_list))
        .route(
            "/consents/{id}",
            post(consents_resolve).delete(consents_deny),
        )
        .route("/attachments/{id}", get(attachment_get))
        .route("/phone/commands", get(phone_commands_list))
        .route("/phone/commands/{id}/result", post(phone_command_result))
        .route("/phones", get(phones_list))
        .route("/phones/{device}", delete(phones_forget))
        .route("/ingest", post(ingest))
        .route(
            "/notifications",
            get(notifications_list).post(notifications_push),
        )
        .route("/tasks", get(tasks_list).post(tasks_add))
        .route("/tasks/{id}", delete(tasks_remove))
        .route("/notifications/{id}/ack", post(notifications_ack))
        .route("/agent", get(agent_get).put(agent_put))
        .route(
            "/agent/{image}",
            get(agent_image_get)
                .put(agent_image_put)
                .delete(agent_image_delete),
        )
        .route("/providers", get(providers_list))
        .route("/providers/active", put(providers_active))
        .route(
            "/providers/{id}/credentials",
            put(providers_set_credentials),
        )
        .route("/providers/{id}", delete(providers_remove))
        .route("/providers/{id}/models", get(providers_models))
        .route(
            "/chat/completions",
            post(crate::openai::chat_completions)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_MESSAGE_BODY_BYTES)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        // The one error shape everywhere, unknown routes included: a
        // client's typo answers JSON it can read, not axum's bare text.
        .fallback(not_found)
        .route(
            "/",
            get(|| async {
                // A bare host hit (a browser on a ui-less deployment, a
                // curl probing the daemon) explains itself instead of
                // 404ing into confusion.
                Json(json!({
                    "service": "rhema-claw",
                    "chat": "/v1/sessions",
                    "api": ["/v1/sessions", "/v1/providers", "/v1/watches", "/v1/calendars",
                            "/v1/accounts", "/v1/notifications", "/v1/phones",
                            "/v1/bootstrap", "/v1/ingest"],
                }))
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .nest("/v1", v1)
        .with_state(state)
}

/// The unknown-route answer: the contract's error shape.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "no such route"})),
    )
        .into_response()
}

/// Byte-length-safe constant-time equality over the two strings' UTF-8
/// bytes: every compared byte is XOR-folded into one difference word
/// along with the length difference, so the answer takes the same work
/// whether the mismatch is in the first byte or the last. Not a MAC —
/// just enough that the bearer check does not leak the token a timing
/// probe at a time.
fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    let mut diff = presented.len() ^ expected.len();
    for (a, b) in presented.iter().zip(expected) {
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

/// The bearer check, in three classes. With no token configured the
/// surface is open, as before. The human token answers every `/v1`
/// route. The agent token — what the client tools hold — answers only
/// the data routes the tool contract allowlists, and only with the
/// contract header at the matching version: a tool cannot reach
/// consents, sessions, or ingest, so a leaked agent token cannot
/// answer the agent's own `ask_secret` (rhema-tools spec §5). The
/// ingest credential — what integrations hold — answers the event
/// door and nothing else: write-only by construction, it can hand
/// events in and read nothing back. Any other bearer is a 401.
/// `/healthz` never reaches this.
async fn require_auth(State(state): State<ClawState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let class = match (
        &state.token,
        &state.agent_token,
        &state.ingest_token,
        presented,
    ) {
        // Nothing configured: the open surface, bearers ignored.
        (None, None, None, _) => Auth::Human,
        (Some(expected), _, _, Some(presented)) if tokens_match(presented, expected) => Auth::Human,
        (_, Some(expected), _, Some(presented)) if tokens_match(presented, expected) => Auth::Agent,
        (_, _, Some(expected), Some(presented)) if tokens_match(presented, expected) => {
            Auth::Ingest
        }
        // A token configured and no bearer, or a bearer matching
        // neither.
        (_, _, _, _) => Auth::Invalid,
    };
    match class {
        Auth::Human => next.run(request).await,
        // Both mean "no credential": Auth::None never leaves the
        // classifier, but the match stays total.
        Auth::None | Auth::Invalid => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or wrong bearer token"})),
        )
            .into_response(),
        Auth::Agent => {
            let version = request
                .headers()
                .get(CONTRACT_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == CONTRACT_VERSION.to_string());
            if !version {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": format!(
                        "agent calls carry the {CONTRACT_HEADER} header at version {CONTRACT_VERSION}"
                    )})),
                )
                    .into_response();
            }
            if rhema_claw_tools_contract::agent_allowed(
                request.method().as_str(),
                request.uri().path(),
            ) {
                next.run(request).await
            } else {
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "this route is not the agent token's"})),
                )
                    .into_response()
            }
        }
        Auth::Ingest => {
            let version = request
                .headers()
                .get(CONTRACT_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == CONTRACT_VERSION.to_string());
            if !version {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": format!(
                        "ingest calls carry the {CONTRACT_HEADER} header at version {CONTRACT_VERSION}"
                    )})),
                )
                    .into_response();
            }
            if rhema_claw_tools_contract::ingest_allowed(
                request.method().as_str(),
                request.uri().path(),
            ) {
                next.run(request).await
            } else {
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "this route is not the ingest token's"})),
                )
                    .into_response()
            }
        }
    }
}

async fn sessions(
    State(state): State<ClawState>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    state
        .sessions()
        .map(Json)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// `{"name": "my-notes"}` — a chat session to create.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateBody {
    /// The name under the `interactive:` prefix.
    pub name: String,
}

/// The session name a create route accepts: the shape a key's suffix
/// must take to stay legible in lists, URLs, and logs.
fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// `POST /v1/sessions`: create an empty `interactive:<name>` session —
/// a record, not a conversation; nothing dispatches until a message
/// arrives. Sessions otherwise come into being implicitly, on their
/// first message, which is why a UI could not offer one before this.
async fn sessions_create(
    State(state): State<ClawState>,
    Json(body): Json<SessionCreateBody>,
) -> Response {
    let name = body.name.trim();
    if !valid_session_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name must be 1-64 chars of letters, digits, '.', '_', or '-'"})),
        )
            .into_response();
    }
    let key = ResourceKey::from(format!("{}{name}", crate::interactive::KEY_PREFIX));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // The CAS precondition does the duplicate check: `None` means the
    // key must not exist yet, so a race between two creates resolves to
    // one 201 and one 409, never two sessions under one key.
    match state.store.save(KeyRecord::fresh(key.clone(), now), None) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({ "key": key.as_str(), "created": true })),
        )
            .into_response(),
        Err(rhema_rsd::StoreError::RevisionMismatch { .. }) => (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("session {:?} already exists", key.as_str())})),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// The setup badge: which of SOUL.md and USER.md exist, read live — a
/// UI badges first-run setup from it while the onboarding offer is
/// still open.
async fn bootstrap_status(
    State(state): State<ClawState>,
) -> Result<Json<crate::bootstrap::BootstrapStatus>, (StatusCode, String)> {
    state
        .bootstrap
        .status()
        .map(Json)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// The decoded budget for one inbound chat image: 5 MiB decoded per
/// image, eight images a message ([`MAX_INBOUND_IMAGES`]) — enough for
/// screenshots, small enough that one message cannot bury the
/// attachment store or a provider's inline limit.
pub const MAX_INBOUND_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// How many images one chat message may carry.
pub const MAX_INBOUND_IMAGES: usize = 8;

/// The messages route's request-body ceiling, derived from the image
/// budget: [`MAX_INBOUND_IMAGES`] images at [`MAX_INBOUND_IMAGE_BYTES`]
/// decoded each, base64-expanded 4:3, plus room for the text. Axum's
/// default 2 MiB would refuse what the budget allows before this
/// route's own check could answer.
pub const MAX_MESSAGE_BODY_BYTES: usize =
    MAX_INBOUND_IMAGE_BYTES * 4 / 3 * MAX_INBOUND_IMAGES + 1024 * 1024;

/// `{"text": "..."}` — one chat message, plus the images it carries.
/// A body without `images` is the old shape, unchanged.
#[derive(Deserialize)]
pub struct MessageBody {
    /// The message text.
    pub text: String,
    /// The message's inline images, each the full MIME type plus bare
    /// base64. The route checks the budget, then the dispatch copies
    /// the bytes into the attachment store — the stream carries ids,
    /// not megabytes.
    #[serde(default)]
    pub images: Vec<WireImage>,
}

/// The inbound-image budget: a media type that parses as an image
/// format, a non-empty base64 body (the decoder, not this check, is
/// the base64 judge), decoded size within [`MAX_INBOUND_IMAGE_BYTES`],
/// at most [`MAX_INBOUND_IMAGES`] a message. The OpenAI shim runs the
/// same check over the same budget.
pub(crate) fn validate_images(images: &[WireImage]) -> Result<(), String> {
    if images.len() > MAX_INBOUND_IMAGES {
        return Err(format!(
            "at most {MAX_INBOUND_IMAGES} images a message, got {}",
            images.len()
        ));
    }
    for image in images {
        if ImageMediaType::parse(&image.media_type).is_none() {
            return Err(format!(
                "unsupported image media type {:?}",
                image.media_type
            ));
        }
        if image.data.trim().is_empty() {
            return Err("image data must not be empty".to_owned());
        }
        // Bare base64: four characters decode to three bytes, so the
        // size check needs no decode.
        if image.data.len() * 3 / 4 > MAX_INBOUND_IMAGE_BYTES {
            return Err(format!(
                "image too large: the decoded budget is {MAX_INBOUND_IMAGE_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

async fn messages(
    State(state): State<ClawState>,
    Path(key): Path<String>,
    Json(body): Json<MessageBody>,
) -> Response {
    if !crate::interactive::is_resident_key(&key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key must start with interactive:, task:, or phone:"})),
        )
            .into_response();
    }
    if body.text.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "text must not be empty"})),
        )
            .into_response();
    }
    if let Err(reason) = validate_images(&body.images) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": reason}))).into_response();
    }
    let key = ResourceKey::from(key);
    // No subscribe-before-ingest here: the hub records every turn
    // whether or not anyone is watching, so a chat surface connecting
    // later replays the whole exchange. The reply itself is consumed on
    // the event stream, not here.
    if let Err(err) = state.dispatcher.ingest(crate::interactive::user_event(
        key.as_str(),
        &body.text,
        &body.images,
    )) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    // Steering: a message into a key whose turn is live cuts that turn
    // short and runs instead of it. After the ingest, per the
    // registry's contract — the signal must never outrun its message.
    // The bool is informational: no live turn means the message simply
    // waits for admission, as it always did.
    state.steer().signal(key.as_str());
    // The drain runs detached: a chat turn is minutes of tool rounds,
    // and this route's contract is acceptance, not completion — the
    // reply arrives on the event stream. Holding the POST for the
    // whole turn would lock the sender's composer (and every message
    // typed behind it) for that long; the daemon's tick is the backstop
    // for a drain a process exit cuts.
    tokio::spawn(drain_owned(
        state.driver.clone(),
        state.dispatcher.clone(),
        key.clone(),
    ));
    (
        StatusCode::ACCEPTED,
        Json(json!({"key": key.as_str(), "accepted": true})),
    )
        .into_response()
}

/// Reads a query parameter the way a JSON body reads `null`: a blank
/// value (`?since=`) means "not set", not "unparseable".
///
/// `Option<T>` alone only forgives a *missing* key. Axum's `Query`
/// hands a present-but-empty parameter through as an empty string, and
/// parsing that into `T` fails the whole request with a 400 — which is
/// what the Android client hit on its first notification poll: it sent
/// its cursor unconditionally, and before the first ack the cursor is
/// empty, so the poll answered
/// `Failed to deserialize query string: since: cannot parse integer from empty string`
/// and the phone showed an error instead of an empty queue. The client
/// now omits the parameter when it has nothing to say, and the door
/// forgives the shape anyway: a query string is a text form, and an
/// empty `since` reads as "from the start" however it got there.
fn blank_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => value
            .parse::<T>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// How much of the record a client with nothing cached opens with: the
/// newest events, cut back to the first turn boundary in the window. A
/// resident session runs for weeks, so replaying it whole to paint a
/// screen would cost more the longer the session lived; the tail paints
/// the latest turn at once and the client walks back through history as
/// the reader scrolls up.
const TAIL_WINDOW: usize = 400;

/// One backward history page: the size asked for by default, and the
/// most one request may ask for.
const HISTORY_PAGE: usize = 400;
const HISTORY_PAGE_MAX: usize = 2000;

/// The events route's query: `since` is the seq of the last event the
/// client already holds, from the `id:` line of the frame that carried
/// it. The replay starts after it, so a reconnecting client receives
/// exactly the gap it missed, whatever its size. Absent, the stream
/// opens with the tail ([`TAIL_WINDOW`]); a blank `since` is absent, see
/// [`blank_as_none`].
#[derive(Debug, Deserialize)]
struct EventStreamQuery {
    #[serde(default, deserialize_with = "blank_as_none")]
    since: Option<i64>,
}

async fn events(
    State(state): State<ClawState>,
    Path(key): Path<String>,
    Query(query): Query<EventStreamQuery>,
) -> Response {
    // The same resident-key rule the messages route applies: an SSE
    // subscription to a forge key is a misuse, not a stream.
    if !crate::interactive::is_resident_key(&key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key must start with interactive:, task:, or phone:"})),
        )
            .into_response();
    }
    // History first, live second, cut atomically: the snapshot and the
    // subscription leave the hub under one lock, so a client folding the
    // replay and then the stream sees every event exactly once. A
    // client holding a cursor opens with `since` and receives only the
    // gap; one with nothing cached opens with the tail, then walks back
    // through `history` for the turns above it.
    let key = ResourceKey::from(key);
    let (replay, mut receiver) = match query.since {
        Some(cursor) => state.hub.replay_after(&key, cursor, TAIL_WINDOW),
        None => state.hub.replay_tail(&key, TAIL_WINDOW),
    };
    let stale = replay.stale;
    // A tail window can open mid-turn (the oldest events in it belong
    // to a turn that started before the window), and a transcript that
    // opens on a reply with no question above it reads as a bug. Cut
    // back to the window's first question. A cursor replay is exact by
    // construction and is never cut.
    let history = if query.since.is_none() {
        snap_to_turn(replay.events)
    } else {
        replay.events
    };
    // The pump decouples the broadcast channel from the SSE body: the
    // client's disconnect closes the body, which fails the send, which
    // ends the pump. The stream never ends on its own — a chat UI
    // subscribes once and chats forever.
    let (sender, body) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        // Every frame carries its seq as the SSE `id:`, so the client's
        // cursor is read off the wire and a reconnect names the exact
        // event it stopped at. The record numbers events densely and
        // the hub appends and broadcasts under one lock, so the seq the
        // pump counts matches the record's: a lagged drop is announced
        // with how many were skipped, and the count steps over them.
        let mut next_seq = match history.last() {
            Some((seq, _)) => seq + 1,
            // A cursor replay with an empty gap names no seq to count
            // from, but the record's next one is the cursor's successor
            // — zero would hand the client a number that names nothing
            // and make its next resume replay the whole record.
            None if !stale => query.since.map_or(0, |cursor| cursor + 1),
            None => 0,
        };
        if stale {
            // The cursor named events the record no longer holds: the
            // replay above is the tail, not the gap. Say so before it,
            // so a client that kept its transcript drops it instead of
            // splicing a reset onto the end of a stale one.
            let reset = SseEvent::default()
                .comment("reset; the record no longer holds the events your cursor named");
            if sender.send(Ok(reset)).await.is_err() {
                return;
            }
        }
        for (seq, turn) in history {
            if sender.send(Ok(sse_event(seq, turn))).await.is_err() {
                return;
            }
        }
        loop {
            let item = match receiver.recv().await {
                Ok(turn) => {
                    let seq = next_seq;
                    next_seq += 1;
                    sse_event(seq, turn)
                }
                Err(RecvError::Lagged(dropped)) => {
                    next_seq += dropped as i64;
                    SseEvent::default()
                        .comment(format!("lagged; {dropped} earlier event(s) dropped"))
                }
                Err(RecvError::Closed) => break,
            };
            if sender.send(Ok(item)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(body))
        .keep_alive(
            KeepAlive::default()
                .interval(KEEP_ALIVE_INTERVAL)
                .text("keep-alive"),
        )
        .into_response()
}

/// `POST /v1/calendars`: one calendar spec, the integration's
/// `CalendarWatch` shape, validated at the table's door — the same
/// rules the config's boot check used to run.
async fn calendars_add(
    State(state): State<ClawState>,
    Json(body): Json<rhema_calendar_integration::CalendarWatch>,
) -> Response {
    match state.calendars.add(&body) {
        Ok(true) => (
            StatusCode::CREATED,
            Json(json!({"added": body.name.trim()})),
        )
            .into_response(),
        Ok(false) => (
            StatusCode::OK,
            Json(json!({
                "added": body.name.trim(),
                "note": "a calendar with that name already exists"
            })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/calendars/{name}`: forget a calendar; the reminders
/// source stops polling it on the next tick.
async fn calendars_remove(State(state): State<ClawState>, Path(name): Path<String>) -> Response {
    let Some(name) = Some(name.trim()).filter(|n| !n.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response();
    };
    match state.calendars.remove(name) {
        Ok(removed) => Json(json!({"removed": removed, "name": name})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/calendars`: every calendar, in creation order.
async fn calendars_list(State(state): State<ClawState>) -> Response {
    match state.calendars.list() {
        Ok(list) => Json(json!({"calendars": list})).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// One `TurnEvent` as an SSE frame: `id: {seq}` then `data: {json}`.
/// The id is the record's seq for the event, which is what a client
/// names as `since` when it reconnects. A lagged subscriber gets a
/// comment naming the drop — live delivery is best-effort; the record
/// replays what completed. A serialization failure is logged with the
/// event kind instead of vanishing: a dropped frame a client can never
/// see must at least reach the operator.
fn sse_event(seq: i64, turn: TurnEvent) -> SseEvent {
    match serde_json::to_string(&turn) {
        Ok(data) => SseEvent::default().id(seq.to_string()).data(data),
        Err(err) => {
            eprintln!(
                "[claw] warning: dropped unserializable {} event: {err}",
                turn.kind()
            );
            SseEvent::default().comment("unserializable event dropped")
        }
    }
}

/// A replay window cut back to its first question: the events before
/// it belong to a turn whose start the window did not reach, and half a
/// turn reads as a bug. A window with no question in it (one turn
/// longer than the window) is returned whole — showing it beats showing
/// nothing, and the walk back through history continues above it.
fn snap_to_turn(mut events: Vec<crate::hub::SeqEvent>) -> Vec<crate::hub::SeqEvent> {
    match events
        .iter()
        .position(|(_, event)| matches!(event, TurnEvent::UserMessage { .. }))
    {
        Some(index) if index > 0 => events.split_off(index),
        _ => events,
    }
}

/// The history route's query: the page ends at `before` (the seq of the
/// oldest event the client holds; absent reads from the newest), and
/// `limit` is the most events it may hold before the page is widened
/// back to a turn boundary — see [`EventHub::replay_turn_before`].
#[derive(Debug, Deserialize)]
struct HistoryQuery {
    #[serde(default, deserialize_with = "blank_as_none")]
    before: Option<i64>,
    #[serde(default, deserialize_with = "blank_as_none")]
    limit: Option<usize>,
}

/// `GET /v1/sessions/{key}/history?before=<seq>&limit=<n>`: one page of
/// turns older than the client's oldest event, for the walk back
/// through a session's history. The page starts on a turn boundary and
/// ends exactly at `before`, so pages stack without gaps or overlaps.
/// The response's `more` says whether anything precedes it: `false`
/// means the client has reached the session's first event.
async fn session_history(
    State(state): State<ClawState>,
    Path(key): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    if !crate::interactive::is_resident_key(&key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key must start with interactive:, task:, or phone:"})),
        )
            .into_response();
    }
    let key = ResourceKey::from(key);
    let limit = query
        .limit
        .unwrap_or(HISTORY_PAGE)
        .clamp(1, HISTORY_PAGE_MAX);
    let page = state.hub.replay_turn_before(&key, query.before, limit);
    // Whether anything sits below the page: a lookup, not a read.
    let more = match page.first().map(|(seq, _)| *seq) {
        Some(first) => state.hub.has_record_before(&key, first),
        None => false,
    };
    let events: Vec<Value> = page
        .into_iter()
        .map(|(seq, event)| json!({"seq": seq, "event": event}))
        .collect();
    Json(json!({"events": events, "more": more})).into_response()
}

async fn watches_list(State(state): State<ClawState>) -> Result<Json<Value>, (StatusCode, String)> {
    let watches = state
        .watches
        .list()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // The account watches ride along: a saved account whose forge
    // notifications are not actually being followed is invisible
    // otherwise — the save's `watch` flag defaults to false on both the
    // tool and this surface, and "is my account subscribed?" is the
    // first question a silent forge thread raises.
    let account_watches = state
        .watches
        .list_account_watches()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // The poll health rides along for the same reason the account
    // watches do: "has this ever polled?" is the first question a silent
    // watch raises, and a tick that failed with a 401 answers it, while
    // an empty table would say nothing either way.
    let health = state
        .watches
        .list_health()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({
        "watches": watches,
        "account_watches": account_watches,
        "health": health,
    })))
}

/// `{"forge", "repo", "host"?, "token"?}` — the shape every watch entry
/// validates the same, wherever it enters.
#[derive(Deserialize)]
pub struct WatchBody {
    /// `github`, `gitea`, or `gitlab`.
    pub forge: String,
    /// `owner/name` (GitLab: `namespace/project`).
    pub repo: String,
    /// Instance base URL; required for gitea.
    pub host: Option<String>,
    /// Forge token; falls back to the forge's environment variable.
    pub token: Option<String>,
}

async fn watches_add(State(state): State<ClawState>, Json(body): Json<WatchBody>) -> Response {
    let Some(kind) = rhema_forge_integration::ForgeKind::parse(&body.forge) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "forge must be github, gitea, or gitlab"})),
        )
            .into_response();
    };
    if let Err(reason) = validate_watch(kind, &body.repo, body.host.as_deref()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": reason.to_string()})),
        )
            .into_response();
    }
    let added = match state.watches.add(
        kind.name(),
        body.repo.trim(),
        body.host.as_deref(),
        body.token.as_deref().map(str::trim).unwrap_or(""),
    ) {
        Ok(added) => added,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };
    let status = if added {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        status,
        Json(json!({
            "added": added,
            "forge": kind.name(),
            "repo": body.repo.trim(),
        })),
    )
        .into_response()
}

async fn watches_remove(
    State(state): State<ClawState>,
    Path((forge, repo)): Path<(String, String)>,
) -> Response {
    let removed = match rhema_forge_integration::ForgeKind::parse(&forge) {
        Some(kind) => state
            .watches
            .remove(kind.name(), repo.trim())
            .map_err(|err| err.to_string()),
        None => Ok(false),
    };
    match removed {
        Ok(removed) => (StatusCode::OK, Json(json!({"removed": removed}))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        )
            .into_response(),
    }
}

/// `{"label", "service", "url"?, "username"?, "secret", "secret_kind"?,
/// "watch"?}` — an account to store. The secret is write-only: it is
/// stored and never served back, by the shape of the store's records.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountBody {
    /// The display name.
    pub label: String,
    /// The service, lowercase: `github`, `gitlab`, `gitea`, or any name.
    pub service: String,
    /// Base URL, for self-hosted instances.
    pub url: Option<String>,
    /// The login name on the service.
    pub username: Option<String>,
    /// The credential to store; never echoed back.
    pub secret: String,
    /// `token` (default) or `password`.
    pub secret_kind: Option<String>,
    /// Also follow this forge identity's notifications. Default false.
    pub watch: Option<bool>,
}

async fn accounts_list(State(state): State<ClawState>) -> Response {
    match state.accounts().list() {
        Ok(accounts) => (StatusCode::OK, Json(json!({ "accounts": accounts }))).into_response(),
        Err(err) => internal_error(err.to_string()).into_response(),
    }
}

async fn accounts_add(State(state): State<ClawState>, Json(body): Json<AccountBody>) -> Response {
    let kind = match crate::accounts_tools::parse_kind(body.secret_kind.as_deref()) {
        Ok(kind) => kind,
        Err(reason) => return bad_request(reason).into_response(),
    };
    let mut draft = rhema_identity::AccountDraft::new(
        body.label,
        body.service,
        rhema_identity::Secret::new(kind, body.secret),
    );
    if let Some(url) = body.url {
        draft = draft.with_url(url);
    }
    if let Some(username) = body.username {
        draft = draft.with_username(username);
    }
    match crate::accounts_tools::save_account(
        state.accounts(),
        state.watches(),
        draft,
        body.watch.unwrap_or(false),
    ) {
        Ok(saved) => (
            StatusCode::CREATED,
            Json(json!({
                "saved": true,
                "id": saved.account.id,
                "label": saved.account.label,
                "service": saved.account.service,
                "secret_kind": saved.account.secret_kind,
                "watched": saved.watch_added,
                "watchable_forge": saved.forge_known,
            })),
        )
            .into_response(),
        Err(reason) => bad_request(reason).into_response(),
    }
}

async fn accounts_remove(State(state): State<ClawState>, Path(id): Path<String>) -> Response {
    match crate::accounts_tools::forget_account(state.accounts(), state.watches(), &id) {
        Ok((forgot, watches_removed)) => (
            StatusCode::OK,
            Json(json!({ "forgot": forgot, "watches_removed": watches_removed })),
        )
            .into_response(),
        Err(reason) => internal_error(reason).into_response(),
    }
}

/// The outstanding consent requests — what a running turn has asked and
/// nobody has answered yet, oldest first. A UI polls this after a
/// reload so a card is never lost to one; the live ask also rides the
/// session's event stream as a `consent_request` event.
async fn consents_list(
    State(state): State<ClawState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let consents = state
        .consents
        .pending()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({ "consents": consents })))
}

/// The answer body: `{"secret": "..."}` for a `secret` request,
/// `{"grant": true|false}` for a `phone` one. The two shapes never mix —
/// the request's kind picks which field means anything.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentBody {
    /// The credential, for a `secret` request. Stored into the identity
    /// store; never echoed, never persisted in the consent row.
    pub secret: Option<String>,
    /// The verdict, for a `phone` request.
    pub grant: Option<bool>,
}

async fn consents_resolve(
    State(state): State<ClawState>,
    Path(id): Path<String>,
    Json(body): Json<ConsentBody>,
) -> Response {
    let consent = match state.consents.get(&id) {
        Ok(Some(consent)) => consent,
        Ok(None) => return not_found_consent(),
        Err(err) => return internal_error(err.to_string()).into_response(),
    };
    if consent.resolved_at.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "consent request already resolved"})),
        )
            .into_response();
    }
    match consent.kind {
        crate::consents::ConsentKind::Secret => {
            let secret = body
                .secret
                .as_deref()
                .map(str::trim)
                .filter(|secret| !secret.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": "body must carry a non-empty secret"})),
                    )
                });
            let secret = match secret {
                Ok(secret) => secret,
                Err(response) => return response.into_response(),
            };
            resolve_secret_consent(&state, &consent, secret).await
        }
        crate::consents::ConsentKind::Phone => {
            let Some(granted) = body.grant else {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": "body must carry a grant boolean"})),
                )
                    .into_response();
            };
            settle_consent(&state, &consent, granted, None).await
        }
    }
}

/// `DELETE /v1/consents/{id}`: deny (or cancel) a pending ask — the
/// tool wakes to an in-band refusal instead of a timeout.
async fn consents_deny(State(state): State<ClawState>, Path(id): Path<String>) -> Response {
    let consent = match state.consents.get(&id) {
        Ok(Some(consent)) => consent,
        Ok(None) => return not_found_consent(),
        Err(err) => return internal_error(err.to_string()).into_response(),
    };
    if consent.resolved_at.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "consent request already resolved"})),
        )
            .into_response();
    }
    settle_consent(&state, &consent, false, None).await
}

/// The 404 shape for a consent id.
fn not_found_consent() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "no such consent request"})),
    )
        .into_response()
}

/// `GET /v1/attachments/{id}`: a stored attachment's bytes, content
/// type from its name. A transcript card's fetch path — the route
/// serves a copy the turn made, so a workspace cleanup cannot break a
/// link the transcript already carried.
async fn attachment_get(State(state): State<ClawState>, Path(id): Path<String>) -> Response {
    match state.attachments.get(&id) {
        Ok(Some((stored, bytes))) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    crate::attachments::content_type(&stored.name).to_owned(),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("inline; filename=\"{}\"", stored.name.replace('"', "")),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such attachment"})),
        )
            .into_response(),
        Err(err) => internal_error(err.to_string()).into_response(),
    }
}

/// The command poll's query: which device queue to read.
#[derive(Deserialize)]
pub struct PhoneCommandQuery {
    /// The device queue to read.
    pub device: String,
}

/// `GET /v1/phones`: the fleet — every device that ever reported, with
/// its self-reported label, both clocks, and its latest observations
/// per topic. The client settings screens read this; `DELETE` below is
/// their management half.
async fn phones_list(
    State(state): State<ClawState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let devices = state.phone.devices().map_err(internal_error)?;
    let mut phones = Vec::with_capacity(devices.len());
    for record in devices {
        let snapshot = state
            .phone
            .snapshot(&record.device)
            .map_err(internal_error)?;
        phones.push(json!({
            "device": record.device,
            "label": record.label,
            "first_seen": record.first_seen,
            "last_seen": record.last_seen,
            "topics": snapshot["topics"],
        }));
    }
    Ok(Json(json!({ "phones": phones })))
}

/// `DELETE /v1/phones/{device}`: retire a device — its observations,
/// its queued commands, and its registry row. Answers `forgotten`
/// rather than 404 on an unknown name: the caller raced another
/// manager, and "already gone" is the outcome it wanted.
async fn phones_forget(
    State(state): State<ClawState>,
    Path(device): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let device = device.trim();
    if device.is_empty() {
        return Err(bad_request("device name must not be empty"));
    }
    let forgotten = state.phone.forget_device(device).map_err(internal_error)?;
    Ok(Json(json!({ "forgotten": forgotten })))
}

/// `GET /v1/phone/commands?device=`: the device's queued commands,
/// oldest first, none of them confirmed yet — the poll list the phone
/// executes from and reports on.
async fn phone_commands_list(
    State(state): State<ClawState>,
    Query(query): Query<PhoneCommandQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let device = query.device.trim();
    if device.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "device must not be empty".to_owned(),
        ));
    }
    let commands = state
        .phone
        .pending_commands(device)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({ "commands": commands })))
}

/// `{"ok": true, "detail": "..."}` — one command's execution report.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneCommandResult {
    /// Whether execution succeeded.
    pub ok: bool,
    /// What happened, when the device has something to say.
    pub detail: Option<String>,
}

/// `POST /v1/phone/commands/{id}/result`: the device's report. Consumed
/// once — a redelivered report is a 409, and the waiting tool saw the
/// first verdict.
async fn phone_command_result(
    State(state): State<ClawState>,
    Path(id): Path<String>,
    Json(body): Json<PhoneCommandResult>,
) -> Response {
    let command = match state.phone.command_by_id(&id) {
        Ok(Some(command)) => command,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no such command"})),
            )
                .into_response();
        }
        Err(err) => return internal_error(err.to_string()).into_response(),
    };
    if command.result_ok.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "command already confirmed"})),
        )
            .into_response();
    }
    let detail = body
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .map(str::to_owned);
    match state.phone.record_result(&id, body.ok, detail.as_deref()) {
        Ok(true) => {
            state.bridge.wake(&id);
            (StatusCode::OK, Json(json!({ "acked": true }))).into_response()
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "command already confirmed"})),
        )
            .into_response(),
        Err(err) => internal_error(err.to_string()).into_response(),
    }
}

/// The secret request's second half: store the credential into the
/// identity store under the draft the ask carried, then resolve the row.
/// A store the identity layer refuses (bad kind, caps) leaves the
/// request pending — the human can retry with a corrected answer.
async fn resolve_secret_consent(
    state: &ClawState,
    consent: &crate::consents::Consent,
    secret: String,
) -> Response {
    let Some(draft) = consent.draft.as_ref() else {
        return internal_error("the consent row carries no account draft").into_response();
    };
    let Some(label) = draft["label"].as_str() else {
        return internal_error("the consent draft carries no label").into_response();
    };
    let Some(service) = draft["service"].as_str() else {
        return internal_error("the consent draft carries no service").into_response();
    };
    let kind = match crate::accounts_tools::parse_kind(draft["secret_kind"].as_str()) {
        Ok(kind) => kind,
        Err(reason) => return internal_error(reason).into_response(),
    };
    let mut account = rhema_identity::AccountDraft::new(
        label.to_owned(),
        service.to_owned(),
        rhema_identity::Secret::new(kind, secret),
    );
    if let Some(url) = draft["url"].as_str() {
        account = account.with_url(url.to_owned());
    }
    if let Some(username) = draft["username"].as_str() {
        account = account.with_username(username.to_owned());
    }
    let stored = match state.accounts().put(account) {
        Ok(account) => account,
        Err(err) => return bad_request(err.to_string()).into_response(),
    };
    settle_consent(state, consent, true, Some(stored.id.clone())).await
}

/// Marks the row resolved, wakes the waiting tool, and tells the
/// session's stream. `account_id` rides only a granted secret request.
async fn settle_consent(
    state: &ClawState,
    consent: &crate::consents::Consent,
    granted: bool,
    account_id: Option<String>,
) -> Response {
    let resolved = if consent.kind == crate::consents::ConsentKind::Secret && granted {
        state
            .consents
            .resolve_secret(&consent.id, account_id.as_deref().unwrap_or(""))
    } else {
        state.consents.resolve_decision(&consent.id, granted)
    };
    match resolved {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "consent request already resolved"})),
            )
                .into_response();
        }
        Err(err) => return internal_error(err.to_string()).into_response(),
    }
    state.bridge.wake(&consent.id);
    state.hub.emit(
        &ResourceKey::from(consent.session_key.clone()),
        TurnEvent::ConsentResolved {
            id: consent.id.clone(),
            granted,
        },
    );
    (
        StatusCode::OK,
        Json(json!({
            "resolved": true,
            "granted": granted,
            "account_id": account_id,
        })),
    )
        .into_response()
}

/// `{"source": "...", "key": "...", "payload": {...}}` — one external
/// event: the phone's reporting door, and any future emitter's.
#[derive(Deserialize)]
pub struct IngestBody {
    /// The source name the event ingests under; registered on first use.
    pub source: String,
    /// The resource key the event dispatches to, verbatim.
    pub key: String,
    /// The event body; the key rides inside it for the handler.
    pub payload: Value,
}

/// The source names the daemon's own assembly registers. `/v1/ingest`
/// cannot re-register them: `register_source` replaces, and a generic
/// passthrough landing over the forge or task key functions would
/// misroute every later event of those sources.
fn is_reserved_source(source: &str) -> bool {
    matches!(
        source,
        crate::interactive::SOURCE | crate::task_source::TASKS_SOURCE
    ) || rhema_forge_integration::ForgeKind::parse(source).is_some()
}

/// The external event's dedup id: a hash of the request's coordinates
/// (key plus canonical payload), so an identical redelivery dedups and
/// any change re-ingests — the caller's retry and the dispatcher's
/// at-least-once contract meet here.
fn dedup_id(key: &str, payload: &Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    payload.to_string().hash(&mut hasher);
    format!("ext-{:016x}", hasher.finish())
}

/// What an ingest result says on the wire: the interactive dispatcher's
/// debounce window is zero, so `DebouncePending` means the event is
/// queued for the next driver pass — "dispatched".
fn ingest_tag(result: IngestResult) -> &'static str {
    match result {
        IngestResult::DeliveredLive => "live",
        IngestResult::DebouncePending => "dispatched",
        IngestResult::Duplicate => "duplicate",
    }
}

impl ClawState {
    /// Ingests one external event into the resident dispatcher under
    /// `source`. The source registers lazily on first use — the same
    /// passthrough key function the resident context's own sources use,
    /// reading `payload["key"]` verbatim with the infallible
    /// unique-key fallback — so a source nobody has seen before needs no
    /// restart and no config.
    pub fn ingest_external(
        &self,
        source: &str,
        key: &str,
        payload: Value,
    ) -> Result<IngestResult, DispatchError> {
        {
            let mut known = self
                .generic_sources
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if known.insert(source.to_owned()) {
                self.dispatcher
                    .register_source(source, crate::interactive::passthrough_key_fn());
            }
        }
        // The key_fn reads the payload: the request's key rides inside
        // it, top-level winning over anything the body carried.
        let mut payload = payload;
        payload["key"] = json!(key);
        let event = rhema_rsd::Event {
            id: dedup_id(key, &payload),
            source: source.to_owned(),
            payload,
            received_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        self.dispatcher.ingest(event)
    }
}

async fn ingest(State(state): State<ClawState>, Json(body): Json<IngestBody>) -> Response {
    let source = body.source.trim();
    if source.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source must not be empty"})),
        )
            .into_response();
    }
    if is_reserved_source(source) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!(
                "source {source:?} is reserved by the daemon's own sources"
            )})),
        )
            .into_response();
    }
    let key = body.key.trim();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key must not be empty"})),
        )
            .into_response();
    }
    if !body.payload.is_object() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "payload must be a JSON object"})),
        )
            .into_response();
    }
    // A topic-shaped phone key (`phone:<device>:<topic>`) is telemetry:
    // it lands in the phone tables and stops there. A dispatched turn
    // per flip would cost a model call and, through auto-notify, a push
    // notification per flip — and the report has nothing to say a
    // `phone status` read cannot. A bare `phone:<device>` key is a
    // message on the device's resident key and dispatches like any
    // resident key's. A store failure is logged, never fatal.
    if source == crate::phone::SOURCE
        && let Some((device, topic)) = crate::phone::phone_key_parts(key)
    {
        if let Err(err) = state.phone.observe(device, topic, &body.payload) {
            eprintln!("[claw] warning: phone state for {key} not stored: {err}");
        }
        return (
            StatusCode::ACCEPTED,
            Json(json!({ "ingested": "observed" })),
        )
            .into_response();
    }
    match state.ingest_external(source, key, body.payload) {
        Ok(result) => (
            StatusCode::ACCEPTED,
            Json(json!({ "ingested": ingest_tag(result) })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// The poll's query: which device, and the last notification id the
/// caller already saw.
#[derive(Deserialize)]
pub struct NotificationQuery {
    /// The device queue to read; the phone when unset.
    pub device: Option<String>,
    /// Only notifications with a greater id; `0` when unset, and a
    /// blank value counts as unset (the poll may send `?since=` before
    /// its first ack).
    #[serde(default, deserialize_with = "blank_as_none")]
    pub since: Option<i64>,
}

/// The device's outstanding notifications, ascending by id. Delivered
/// rows stop answering once acked; an unacked row keeps coming back, so
/// a poll-and-ack device sees every notification at least once.
async fn notifications_list(
    State(state): State<ClawState>,
    Query(query): Query<NotificationQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let device = query
        .device
        .as_deref()
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .unwrap_or(crate::notifications::DEFAULT_DEVICE);
    let since = query.since.unwrap_or(0);
    let notifications = state
        .notifications
        .pending(device, since)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({ "notifications": notifications })))
}

/// The body `POST /v1/notifications` takes: the client tools' push.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationPushBody {
    title: String,
    body: String,
    device: Option<String>,
}

/// The client tools' push: the same queue the notification poll
/// serves, entered over the surface — one row, delivered at the
/// device's next poll-and-ack (rhema-tools spec §6). The route the
/// `POST /v1/notifications` client speaks.
async fn notifications_push(
    State(state): State<ClawState>,
    Json(body): Json<NotificationPushBody>,
) -> Response {
    if body.title.trim().is_empty() || body.body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "title and body must not be empty"})),
        )
            .into_response();
    }
    let device = body
        .device
        .as_deref()
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .unwrap_or(crate::notifications::DEFAULT_DEVICE);
    match state
        .notifications
        .push(device, body.title.trim(), body.body.trim())
    {
        Ok(record) => (StatusCode::OK, Json(record)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// The body `POST /v1/tasks` takes: the client tools' schedule. `kind`
/// picks the shape, exactly the two the resident scheduler fires.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskAddBody {
    kind: String,
    prompt: String,
    at_millis: Option<i64>,
    interval_millis: Option<i64>,
    deliver_to: Option<String>,
}

/// The scheduled-task table, oldest first — the same rows the
/// resident's tick fires.
async fn tasks_list(State(state): State<ClawState>) -> Result<Json<Value>, (StatusCode, String)> {
    let tasks = state
        .tasks
        .list()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({ "tasks": tasks })))
}

/// The client tools' schedule: one task, fired by the resident's tick
/// when due (rhema-tools spec §6). The route the `POST /v1/tasks`
/// client speaks.
async fn tasks_add(State(state): State<ClawState>, Json(body): Json<TaskAddBody>) -> Response {
    if body.prompt.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "prompt must not be empty"})),
        )
            .into_response();
    }
    let now = now_millis();
    let scheduled = match body.kind.as_str() {
        "at" => {
            let Some(at_millis) = body.at_millis else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "a one-shot task needs at_millis"})),
                )
                    .into_response();
            };
            state.tasks.schedule_at(
                body.prompt.trim(),
                at_millis,
                now,
                body.deliver_to.as_deref(),
            )
        }
        "every" => {
            let Some(interval_millis) = body.interval_millis else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "a recurring task needs interval_millis"})),
                )
                    .into_response();
            };
            if interval_millis <= 0 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "interval_millis must be positive"})),
                )
                    .into_response();
            }
            state.tasks.schedule_every(
                body.prompt.trim(),
                interval_millis,
                now,
                body.deliver_to.as_deref(),
            )
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "kind must be at or every"})),
            )
                .into_response();
        }
    };
    match scheduled {
        Ok(record) => (StatusCode::OK, Json(record)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// Milliseconds since the Unix epoch — the task table's clock.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Cancels one task. Like the watch removal, the answer is
/// `{"removed": false}` for an id already gone — a retried cancel
/// converges instead of 404ing.
async fn tasks_remove(State(state): State<ClawState>, Path(id): Path<i64>) -> Response {
    match state.tasks.remove(id) {
        Ok(removed) => (StatusCode::OK, Json(json!({"removed": removed}))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// Marks one notification delivered. Idempotent: `false` when the id is
/// unknown or already acked.
async fn notifications_ack(State(state): State<ClawState>, Path(id): Path<i64>) -> Response {
    match state.notifications.ack(id) {
        Ok(acked) => (StatusCode::OK, Json(json!({ "acked": acked }))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// Drains the interactive dispatcher around one triggered message: run
/// passes until one admits nothing and `key` has nothing waiting, capped
/// at [`MAX_DRAIN_PASSES`] — a message delivered live into a running
/// turn waits for the daemon's tick instead of a fourth HTTP digit.
pub(crate) async fn drain(
    driver: &Driver,
    dispatcher: &Dispatcher,
    key: &ResourceKey,
) -> Result<(), DispatchError> {
    for _ in 0..MAX_DRAIN_PASSES {
        if driver.run_once().await?.is_empty() {
            let status = dispatcher.status(key)?;
            if status.inbox_depth == 0 && status.state != KeyState::Reacquire {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// [`drain`] over owned handles: the shape the messages route spawns,
/// since a detached pass outlives the request that started it. A
/// failure is logged, not delivered — the caller already got its 202.
pub(crate) async fn drain_owned(
    driver: Arc<Driver>,
    dispatcher: Arc<Dispatcher>,
    key: ResourceKey,
) {
    if let Err(err) = drain(&driver, &dispatcher, &key).await {
        eprintln!("[claw] warning: detached drain for {key} failed: {err}");
    }
}

/// `GET /v1/providers`: every constructible backend and the active pair.
/// The active pair is runtime state — the config file seeded it, the
/// settings store owns it now. Each provider is one instance, named by
/// its `id` (the kind unless a login named it), and carries `source`,
/// where its credential resolves from: `env` (returns on restart even
/// after a removal), `stored` (the credentials this surface persisted),
/// `profile` (the stored rhema config), or `keyless`. `addable` lists
/// the kinds a runtime login can add and the fields their form takes.
async fn providers_list(State(state): State<ClawState>) -> Json<Value> {
    Json(providers_json(&state))
}

/// The providers state as the wire sees it: the shared body of the list
/// route and of every mutation that answers with the new state.
fn providers_json(state: &ClawState) -> Value {
    let (provider, model) = state.backends().active();
    json!({
        "active": {"provider": provider, "model": model},
        "providers": state.backends().list()
            .into_iter()
            .map(|row| json!({
                "id": row.id,
                "kind": row.kind,
                "default_model": row.default_model,
                "source": credential_source(state, &row.id, &row.kind),
            }))
            .collect::<Vec<_>>(),
        "addable": crate::backends::addable_kinds()
            .iter()
            .map(|kind| json!({
                "kind": kind,
                "fields": crate::backends::param_fields_for(kind),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Where an instance's credential resolves from, at this moment: env
/// wins, then the surface's stored credentials, then the stored rhema
/// config; llama.cpp serves keyless. Never the key itself.
fn credential_source(state: &ClawState, id: &str, kind: &str) -> &'static str {
    if crate::backends::env_key_names(kind)
        .iter()
        .any(|name| std::env::var(name).is_ok())
    {
        return "env";
    }
    if state
        .settings()
        .get(&crate::backends::provider_settings_key(id))
        .ok()
        .flatten()
        .is_some()
    {
        return "stored";
    }
    if kind == "llama-cpp" {
        return "keyless";
    }
    "profile"
}

/// `PUT /v1/providers/{id}/credentials`: add a backend instance at
/// runtime, or replace an existing id's credentials. The body carries
/// the kind's fields (`api_key`, and `url` for llama.cpp) plus the
/// `kind` — required when the id is new and refused when it names an
/// instance of another kind. The backend constructs immediately — an
/// unreachable server or a wrong key surfaces on the next turn, not
/// here — and the credentials persist in the settings store, so the
/// next boot discovers them. The active pair is untouched.
async fn providers_set_credentials(
    State(state): State<ClawState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let id = id.trim();
    if !crate::backends::valid_instance_id(id) {
        return Err(bad_request(
            "provider name must be 1-64 chars with no whitespace or '/'",
        ));
    }
    let kind = match state.backends().get(id) {
        Some(existing) => {
            if let Some(kind) = body["kind"].as_str()
                && kind != existing.kind
            {
                return Err(bad_request(format!(
                    "provider {id:?} is a {} backend, not {kind:?}",
                    existing.kind
                )));
            }
            existing.kind.clone()
        }
        None => match body["kind"].as_str() {
            Some(kind) if crate::backends::addable_kinds().contains(&kind) => kind.to_owned(),
            Some(kind) => return Err(bad_request(format!("unknown provider kind {kind:?}"))),
            None => {
                return Err(bad_request(format!(
                    "unknown provider {id:?}: its first login must carry a kind"
                )));
            }
        },
    };
    let blank_is_absent = |value: Option<&str>| {
        value
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty())
    };
    let params = crate::backends::ProviderParams {
        kind: kind.clone(),
        api_key: blank_is_absent(body["api_key"].as_str()),
        url: blank_is_absent(body["url"].as_str()),
    };
    let mut backend = crate::backends::construct(&kind, &params).map_err(bad_request)?;
    backend.id = id.to_owned();
    state.backends().insert(backend);
    let stored = serde_json::to_string(&params).map_err(internal_error)?;
    state
        .settings()
        .set(&crate::backends::provider_settings_key(id), &stored)
        .map_err(internal_error)?;
    Ok(Json(providers_json(&state)))
}

/// `DELETE /v1/providers/{id}`: remove a backend instance from the live
/// registry and drop its stored credentials. An env-resolved credential
/// re-discovers on restart — the list's `source` says so. Removing the
/// last backend is refused; when the removed instance was active, the
/// first remaining backend takes over and the persisted pair moves with
/// it.
async fn providers_remove(
    State(state): State<ClawState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state.backends().remove(&id).map_err(bad_request)?;
    state
        .settings()
        .delete(&crate::backends::provider_settings_key(&id))
        .map_err(internal_error)?;
    let (provider, model) = state.backends().active();
    state
        .settings()
        .set(crate::settings::ACTIVE_PROVIDER_KEY, &provider)
        .map_err(internal_error)?;
    state
        .settings()
        .set(crate::settings::ACTIVE_MODEL_KEY, &model)
        .map_err(internal_error)?;
    Ok(Json(providers_json(&state)))
}

/// `PUT /v1/providers/active`: switch the backend instance the next
/// dispatch runs on, and persist the choice across restarts. An unknown
/// instance id is a 400; a model left unset falls back to its default.
async fn providers_active(
    State(state): State<ClawState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(provider) = body["provider"].as_str() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "body must carry a provider string"})),
        ));
    };
    let model = body["model"].as_str();
    if let Err(reason) = state.backends().set_active(provider, model) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": reason.to_string()})),
        ));
    }
    state
        .settings()
        .set(crate::settings::ACTIVE_PROVIDER_KEY, provider)
        .map_err(internal_error)?;
    let (_, active_model) = state.backends().active();
    state
        .settings()
        .set(crate::settings::ACTIVE_MODEL_KEY, &active_model)
        .map_err(internal_error)?;
    Ok(Json(json!(
        {"active": {"provider": provider, "model": active_model}}
    )))
}

/// `GET /v1/providers/{id}/models`: the instance's live listing,
/// straight from the provider, the cards serialized as rhema-provider's
/// `ModelCard` spells them. `supports_vision` rides when the adapter
/// knows — `true` or `false` — and is absent when it does not: unknown
/// stays absent, never `false`, and a client reads the key's absence as
/// no verdict at all. An id without a listing (unknown, or the mock) is
/// a 404.
async fn providers_models(
    State(state): State<ClawState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(listing) = state.backends().list_models(&id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("no live listing for provider {id:?}")})),
        ));
    };
    let models = listing.await.map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("model listing failed: {err}")})),
        )
    })?;
    Ok(Json(json!({ "models": models })))
}

/// `GET /v1/agent`: the agent's presentation identity — the text fields
/// (each absent when unset) and whether the images are set.
async fn agent_get(
    State(state): State<ClawState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let identity = state.identity().get().map_err(internal_error)?;
    Ok(Json(
        serde_json::to_value(&identity).map_err(internal_error)?,
    ))
}

/// `PUT /v1/agent`: save the text fields. A field the body carries is
/// set (an empty string clears it); an absent field is left alone, so
/// a partial save cannot wipe what it did not mean to touch.
async fn agent_put(
    State(state): State<ClawState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Absent fields carry the current value forward (a partial save
    // touches only what it names); a present empty string clears.
    let current = state.identity().get().map_err(internal_error)?;
    let field = |body_value: Option<&str>, stored: &Option<String>| {
        body_value.map(str::to_owned).or_else(|| stored.clone())
    };
    let texts = crate::identity::IdentityTexts {
        name: field(body["name"].as_str(), &current.name),
        mark: field(body["mark"].as_str(), &current.mark),
        initials: field(body["initials"].as_str(), &current.initials),
        tagline: field(body["tagline"].as_str(), &current.tagline),
        signoff: field(body["signoff"].as_str(), &current.signoff),
    };
    let identity = state.identity().set_texts(&texts).map_err(bad_request)?;
    Ok(Json(
        serde_json::to_value(&identity).map_err(internal_error)?,
    ))
}

/// `PUT /v1/agent/{image}`: store the avatar or logo from the raw body,
/// content type from the header. Answers the updated identity.
async fn agent_image_put(
    State(state): State<ClawState>,
    Path(kind): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let kind = image_kind(&kind)?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok().map(str::to_owned))
        .unwrap_or_default();
    let identity = state
        .identity()
        .set_image(kind, &body, &content_type)
        .map_err(|err| match err {
            crate::identity::IdentityError::NotAnImage(_)
            | crate::identity::IdentityError::TooLarge(_, _) => bad_request(err),
            crate::identity::IdentityError::Backend(_) => internal_error(err),
        })?;
    Ok(Json(
        serde_json::to_value(&identity).map_err(internal_error)?,
    ))
}

/// `GET /v1/agent/{image}`: the image bytes back, content type with
/// them. 404 when unset — the identity document's booleans are how a
/// client knows before asking.
async fn agent_image_get(
    State(state): State<ClawState>,
    Path(kind): Path<String>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let kind = image_kind(&kind)?;
    match state.identity().image(kind).map_err(internal_error)? {
        Some((bytes, content_type)) => {
            Ok(([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response())
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no image set"})),
        )),
    }
}

/// `DELETE /v1/agent/{image}`: drop the image. Answers the identity.
async fn agent_image_delete(
    State(state): State<ClawState>,
    Path(kind): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let kind = image_kind(&kind)?;
    let identity = state.identity().clear_image(kind).map_err(internal_error)?;
    Ok(Json(
        serde_json::to_value(&identity).map_err(internal_error)?,
    ))
}

fn image_kind(name: &str) -> Result<crate::identity::ImageKind, (StatusCode, Json<Value>)> {
    match name {
        "avatar" => Ok(crate::identity::ImageKind::Avatar),
        "logo" => Ok(crate::identity::ImageKind::Logo),
        _ => Err(bad_request(format!("unknown identity image {name:?}"))),
    }
}

/// The 400 shape: the request named a kind or fields the registry
/// cannot construct.
fn bad_request(reason: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": reason.to_string()})),
    )
}

/// The 500 shape: the daemon's own machinery failed — no upstream
/// exists for these routes, so a 502 would misname the failure.
fn internal_error(err: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": err.to_string()})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_shape_of_a_session_entry_is_stable() {
        // The sessions route serializes KeyState/Outcome straight from
        // the dispatcher's vocabulary; pin the spellings clients see.
        assert_eq!(serde_json::to_value(KeyState::Idle).unwrap(), "idle");
        assert_eq!(
            serde_json::to_value(KeyState::Reacquire).unwrap(),
            "reacquire"
        );
        assert_eq!(
            serde_json::to_value(rhema_rsd::Outcome::Handled).unwrap(),
            "Handled"
        );
    }

    #[test]
    fn an_ingest_result_reads_as_the_wire_tag() {
        assert_eq!(ingest_tag(IngestResult::DebouncePending), "dispatched");
        assert_eq!(ingest_tag(IngestResult::DeliveredLive), "live");
        assert_eq!(ingest_tag(IngestResult::Duplicate), "duplicate");
    }

    #[test]
    fn a_dedup_id_is_stable_per_coordinates_and_moves_with_the_payload() {
        let payload = json!({"text": "hello"});
        let first = dedup_id("phone:ana", &payload);
        assert_eq!(first, dedup_id("phone:ana", &payload), "same post, same id");
        assert_ne!(
            first,
            dedup_id("phone:ana", &json!({"text": "hello again"})),
            "a different payload re-ingests"
        );
        assert_ne!(
            first,
            dedup_id("phone:bob", &payload),
            "a different key re-ingests"
        );
    }

    #[test]
    fn the_bearer_comparison_agrees_with_equality() {
        for (presented, expected, matches) in [
            ("sekrit", "sekrit", true),
            ("", "", true),
            ("sekrit", "sekri", false),
            ("sekri", "sekrit", false),
            ("sekrit", "sekritx", false),
            ("wrong", "sekrit", false),
            ("sékrit", "sekrit", false),
        ] {
            assert_eq!(
                tokens_match(presented, expected),
                matches,
                "{presented:?} vs {expected:?}"
            );
        }
    }

    #[test]
    fn the_daemons_own_source_names_are_reserved() {
        for name in ["github", "gitea", "gitlab", "user", "tasks"] {
            assert!(is_reserved_source(name), "{name} is reserved");
        }
        for name in ["phone", "weather", "telegram"] {
            assert!(!is_reserved_source(name), "{name} is free");
        }
    }
}
