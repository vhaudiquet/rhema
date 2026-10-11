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
use rhema_rsd::{
    DispatchError, Dispatcher, IngestResult, KeyState, ResourceKey, SqliteStore, Store,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::ReceiverStream;

use crate::backends::BackendRegistry;
use crate::bootstrap::Bootstrap;
use crate::hub::{EventHub, TurnEvent};
use crate::notifications::NotificationStore;
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
    notifications: Arc<NotificationStore>,
    bootstrap: Arc<Bootstrap>,
    hub: Arc<EventHub>,
    backends: Arc<BackendRegistry>,
    settings: Arc<crate::settings::SettingsStore>,
    identity: Arc<crate::identity::IdentityStore>,
    token: Option<String>,
    /// The generic sources `/v1/ingest` has registered on the
    /// dispatcher, so a source name registers exactly once however many
    /// events race in through it.
    generic_sources: Arc<Mutex<BTreeSet<String>>>,
}

impl ClawState {
    /// The state over the interactive dispatcher and its driver, the
    /// shared dispatch store, the watch table, the notification table,
    /// the bootstrap store, the event hub, and the configured `model`
    /// (the shim's echo default) and bearer `token`.
    #[allow(clippy::too_many_arguments)] // ten parts, each load-bearing
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        driver: Arc<Driver>,
        store: Arc<SqliteStore>,
        watches: Arc<ClawStore>,
        notifications: Arc<NotificationStore>,
        bootstrap: Arc<Bootstrap>,
        hub: Arc<EventHub>,
        backends: Arc<BackendRegistry>,
        settings: Arc<crate::settings::SettingsStore>,
        identity: Arc<crate::identity::IdentityStore>,
        token: Option<String>,
    ) -> Self {
        Self {
            dispatcher,
            driver,
            store,
            watches,
            notifications,
            bootstrap,
            hub,
            backends,
            settings,
            identity,
            token,
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
}

/// The app router: `/healthz` open, `/v1` behind the bearer check when
/// the config set a token.
pub fn router(state: ClawState) -> Router {
    let v1 = Router::new()
        .route("/sessions", get(sessions))
        .route("/sessions/{key}/messages", post(messages))
        .route("/sessions/{key}/events", get(events))
        .route("/bootstrap", get(bootstrap_status))
        .route("/watches", get(watches_list).post(watches_add))
        .route("/watches/{forge}/{*repo}", delete(watches_remove))
        .route("/ingest", post(ingest))
        .route("/notifications", get(notifications_list))
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
        .route("/chat/completions", post(crate::openai::chat_completions))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route(
            "/",
            get(|| async {
                // A bare host hit (a browser on a ui-less deployment, a
                // curl probing the daemon) explains itself instead of
                // 404ing into confusion.
                Json(json!({
                    "service": "rhema-claw",
                    "chat": "/v1/sessions",
                    "api": ["/v1/sessions", "/v1/providers", "/v1/watches",
                            "/v1/notifications", "/v1/bootstrap", "/v1/ingest"],
                }))
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .nest("/v1", v1)
        .with_state(state)
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

/// The bearer check: with no configured token the surface is open; with
/// one, `Authorization: Bearer <token>` is required on every `/v1`
/// route. `/healthz` never reaches this.
async fn require_auth(State(state): State<ClawState>, request: Request, next: Next) -> Response {
    let authorized = match &state.token {
        None => true,
        Some(expected) => request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .strip_prefix("Bearer ")
                    .is_some_and(|presented| tokens_match(presented, expected))
            }),
    };
    if authorized {
        next.run(request).await
    } else {
        // A body, however small: a silent 401 reads as a network fault
        // to every client debugging its token.
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or wrong bearer token"})),
        )
            .into_response()
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

/// `{"text": "..."}` — one chat message.
#[derive(Deserialize)]
pub struct MessageBody {
    /// The message text.
    pub text: String,
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
    let key = ResourceKey::from(key);
    // No subscribe-before-ingest here: the hub records every turn
    // whether or not anyone is watching, so a chat surface connecting
    // later replays the whole exchange. The reply itself is consumed on
    // the event stream, not here.
    if let Err(err) = state
        .dispatcher
        .ingest(crate::interactive::user_event(key.as_str(), &body.text))
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    if let Err(err) = drain(&state.driver, &state.dispatcher, &key).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"key": key.as_str(), "accepted": true})),
    )
        .into_response()
}

async fn events(State(state): State<ClawState>, Path(key): Path<String>) -> Response {
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
    // replay and then the stream sees every event exactly once. A reload
    // rebuilds its transcript from this — nothing else backfills it.
    let (history, mut receiver) = state.hub.subscribe_with_history(&ResourceKey::from(key));
    // The pump decouples the broadcast channel from the SSE body: the
    // client's disconnect closes the body, which fails the send, which
    // ends the pump. The stream never ends on its own — a chat UI
    // subscribes once and chats forever.
    let (sender, body) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        for turn in history {
            if sender.send(Ok(sse_event(turn))).await.is_err() {
                return;
            }
        }
        loop {
            let item = match receiver.recv().await {
                Ok(turn) => sse_event(turn),
                Err(RecvError::Lagged(dropped)) => SseEvent::default()
                    .comment(format!("lagged; {dropped} earlier event(s) dropped")),
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

/// One `TurnEvent` as an SSE frame: `data: {json}`. A lagged subscriber
/// gets a comment naming the drop — live delivery is best-effort; the
/// record replays what completed. A serialization failure is logged with
/// the event kind instead of vanishing: a dropped frame a client can
/// never see must at least reach the operator.
fn sse_event(turn: TurnEvent) -> SseEvent {
    match serde_json::to_string(&turn) {
        Ok(data) => SseEvent::default().data(data),
        Err(err) => {
            eprintln!(
                "[claw] warning: dropped unserializable {} event: {err}",
                turn.kind()
            );
            SseEvent::default().comment("unserializable event dropped")
        }
    }
}

async fn watches_list(State(state): State<ClawState>) -> Result<Json<Value>, (StatusCode, String)> {
    let watches = state
        .watches
        .list()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(json!({ "watches": watches })))
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
    let Some(kind) = crate::config::ForgeKind::parse(&body.forge) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "forge must be github, gitea, or gitlab"})),
        )
            .into_response();
    };
    if let Err(reason) = validate_watch(kind, &body.repo, body.host.as_deref()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": reason}))).into_response();
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
    let removed = match crate::config::ForgeKind::parse(&forge) {
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
    ) || crate::config::ForgeKind::parse(source).is_some()
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
    /// Only notifications with a greater id; `0` when unset.
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
            if let Some(kind) = body["kind"].as_str() {
                if kind != existing.kind {
                    return Err(bad_request(format!(
                        "provider {id:?} is a {} backend, not {kind:?}",
                        existing.kind
                    )));
                }
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
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": reason}))));
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
/// straight from the provider. An id without a listing (unknown, or the
/// mock) is a 404.
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
