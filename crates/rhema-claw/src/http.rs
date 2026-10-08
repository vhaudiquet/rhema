//! The HTTP surface: the resident agent's chat routes, its session
//! inventory, and its watch CRUD — plus the health probe that stays
//! open when everything else is tokened.
//!
//! The shape of a chat request, end to end: subscribe the hub FIRST
//! (nothing emitted before a subscriber exists is ever streamed to it),
//! ingest the message, then drain the interactive dispatcher — run
//! passes until one admits nothing and the key has nothing waiting — so
//! queued follow-ups drain before the HTTP answer returns. The reply
//! itself arrives on the event stream, not in the POST answer: the POST
//! only says the message was accepted.
//!
//! The drain loop is capped. A pass that runs a foreign key's turn is
//! still a pass; a pathological backlog stops at
//! [`MAX_DRAIN_PASSES`] and leaves the rest to the daemon's tick, which
//! drains the interactive dispatcher as its backstop.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use rhema_autonomous_agent::Driver;
use rhema_rsd::{DispatchError, Dispatcher, KeyState, ResourceKey, SqliteStore, Store};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::ReceiverStream;

use crate::hub::{EventHub, TurnEvent};
use crate::interactive::KEY_PREFIX;
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
    hub: Arc<EventHub>,
    model: String,
    token: Option<String>,
}

impl ClawState {
    /// The state over the interactive dispatcher and its driver, the
    /// shared dispatch store, the watch table, the event hub, and the
    /// configured `model` (the shim's echo default) and bearer `token`.
    pub fn new(
        dispatcher: Arc<Dispatcher>,
        driver: Arc<Driver>,
        store: Arc<SqliteStore>,
        watches: Arc<ClawStore>,
        hub: Arc<EventHub>,
        model: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        Self {
            dispatcher,
            driver,
            store,
            watches,
            hub,
            model: model.into(),
            token,
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
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured bearer token, when the surface is guarded.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

/// The app router: `/healthz` open, `/v1` behind the bearer check when
/// the config set a token.
pub fn router(state: ClawState) -> Router {
    let v1 = Router::new()
        .route("/sessions", get(sessions))
        .route("/sessions/{key}/messages", post(messages))
        .route("/sessions/{key}/events", get(events))
        .route("/watches", get(watches_list).post(watches_add))
        .route("/watches/{forge}/{*repo}", delete(watches_remove))
        .route("/chat/completions", post(crate::openai::chat_completions))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/v1", v1)
        .with_state(state)
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
                    .is_some_and(|presented| presented == expected)
            }),
    };
    if authorized {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
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
    if !key.starts_with(KEY_PREFIX) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("key must start with {KEY_PREFIX:?}")})),
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
    // Subscribe before ingesting: the hub channel exists the moment the
    // turn may start emitting, and holding the receiver across the drain
    // keeps it alive for a subscriber that connects mid-turn. The reply
    // itself is consumed on the event stream, not here.
    let _session_receiver = state.hub.subscribe(&key);
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

async fn events(
    State(state): State<ClawState>,
    Path(key): Path<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let mut receiver = state.hub.subscribe(&ResourceKey::from(key));
    // The pump decouples the broadcast channel from the SSE body: the
    // client's disconnect closes the body, which fails the send, which
    // ends the pump. The stream never ends on its own — a chat UI
    // subscribes once and chats forever.
    let (sender, body) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);
    tokio::spawn(async move {
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
    Sse::new(ReceiverStream::new(body)).keep_alive(
        KeepAlive::default()
            .interval(KEEP_ALIVE_INTERVAL)
            .text("keep-alive"),
    )
}

/// One `TurnEvent` as an SSE frame: `data: {json}`. A lagged subscriber
/// gets a comment naming the drop — the stream is best-effort; the
/// session is the record.
fn sse_event(turn: TurnEvent) -> SseEvent {
    match serde_json::to_string(&turn) {
        Ok(data) => SseEvent::default().data(data),
        Err(_) => SseEvent::default().comment("unserializable event dropped"),
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
}
