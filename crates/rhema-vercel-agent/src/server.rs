//! The HTTP surface: the DO's routes, on a plain server.
//!
//! Same paths, same protocol, one session id per URL segment instead
//! of one Durable Object per id: `GET /healthz`, `GET /session/:id`
//! (the checkpointed record), `POST /config/:id` (the per-session
//! [`SessionConfig`]), `GET /ws/:id` (the WebSocket pump). Turns run
//! exactly the DO's shape — assemble from durable state, checkpoint at
//! the commit hook, checkpoint after the turn — with the platform's
//! differences where they must be: real tasks instead of `spawn_local`
//! (`Send` futures, a tokio one-shot for approvals), tokio timers, and
//! one connection per session: a second socket for a live session is
//! refused, because approvals route to one client. The sandbox a turn
//! runs in is the deployment's choice
//! ([`SandboxSource`], selected once at startup).
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rhema_agent_tools::DenyAll;
use rhema_cloud_agent::wire::{ClientMessage, ServerMessage};
use rhema_cloud_agent::{AgentAssembly, Sandbox, SessionConfig};
use rhema_core::{CommitError, EventSink, ModelProvider, Session};
use rhema_provider::Retrying;
use rhema_provider_anthropic::{AnthropicProvider, ReqwestTransport};
use rhema_rsd::{ResourceKey, Store};
use rhema_sandbox_http::HttpSandbox;
use tokio::sync::{mpsc, oneshot};

use crate::backend::SandboxBackend;
use crate::pg::PgStore;
use crate::sandbox::VercelSandbox;

/// How long an approval ask waits for its client before the gate
/// denies. The client sees `approval_expired` when it happens.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything the routes and turns share.
pub struct AppState {
    /// The checkpoint store.
    pub store: PgStore,
    /// The sandbox a turn's `bash` runs in: the deployment's backend
    /// choice, resolved once at startup.
    pub sandbox: SandboxSource,
    /// The provider key: a wrangler-equivalent secret, from the env.
    pub anthropic_key: String,
    /// The sessions with a live socket. One connection per session:
    /// approvals route to one client, and a second pump would steal
    /// the answers.
    pub connected: Mutex<std::collections::HashSet<String>>,
}

impl AppState {
    /// The shared state for `store`, the selected sandbox backend, and
    /// the provider key.
    pub fn new(store: PgStore, sandbox: SandboxSource, anthropic_key: String) -> Self {
        Self {
            store,
            sandbox,
            anthropic_key,
            connected: Mutex::new(std::collections::HashSet::new()),
        }
    }
}

/// The sandbox clients of one session, and the deployment's choice
/// between them: the Vercel REST sandbox, and — under
/// `SANDBOX_BACKEND=http` — the shared [`HttpSandbox`] to a daemon
/// speaking the sandbox protocol.
///
/// Built once at startup, where the choice is made; every turn asks it
/// for its session's client, so the turn's shell and seeding cannot
/// tell the backends apart — one [`Sandbox`] seam either way.
pub enum SandboxSource {
    /// The Vercel Sandbox REST client: one named sandbox per session.
    Vercel(Arc<dyn crate::api::SandboxApi>),
    /// The protocol daemon's base URL and shared secret; the session
    /// name rides the path.
    Http {
        /// The daemon's base URL, no trailing slash.
        url: String,
        /// The shared secret the daemon checks on every route.
        token: String,
    },
}

impl SandboxSource {
    /// Applies the startup selection: under [`SandboxBackend::Http`]
    /// `vercel_api` never runs, so the REST client is never built and
    /// its token is never demanded — the Vercel API is never called.
    pub fn select(
        backend: SandboxBackend,
        vercel_api: impl FnOnce() -> Arc<dyn crate::api::SandboxApi>,
    ) -> Self {
        match backend {
            SandboxBackend::Vercel => Self::Vercel(vercel_api()),
            SandboxBackend::Http { url, token } => Self::Http { url, token },
        }
    }

    /// The session's sandbox client: the session name is the same id
    /// the checkpoints live under, whichever backend carries it.
    fn for_session(&self, name: &str, workspace: &str) -> Arc<dyn Sandbox> {
        match self {
            Self::Vercel(api) => Arc::new(VercelSandbox::new(
                api.clone(),
                name.to_owned(),
                workspace.to_owned(),
            )),
            Self::Http { url, token } => Arc::new(HttpSandbox::new(url.clone(), name, token)),
        }
    }
}

/// One live connection's shared state: the pump and the turn talk
/// only through these — the outbound queue, the pending approvals,
/// the turn's cancel flag, the busy flag.
struct Live {
    outgoing: mpsc::UnboundedSender<ServerMessage>,
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    cancelled: Arc<AtomicBool>,
    busy: AtomicBool,
}

impl Live {
    /// Queues one message for the socket writer.
    fn send(&self, message: ServerMessage) {
        let _ = self.outgoing.send(message);
    }
}

/// The sink the engine streams through.
struct LiveSink {
    live: Arc<Live>,
}

impl EventSink for LiveSink {
    fn emit(&mut self, event: rhema_core::Event<'_>) {
        if let Some(message) = ServerMessage::from_event(event) {
            self.live.send(message);
        }
    }
}

/// The approval channel: asks over the live connection, denies at the
/// timeout. The native twin of the DO's `WsChannel` — a tokio
/// one-shot and a tokio timer instead of the `!Send` bridge.
struct LiveChannel {
    live: Arc<Live>,
    timeout: Duration,
}

#[async_trait::async_trait]
impl rhema_cloud_agent::ApprovalChannel for LiveChannel {
    async fn ask(&self, call: &rhema_core::ToolCall) -> Option<bool> {
        let (sender, receiver) = oneshot::channel();
        self.live
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call.id.clone(), sender);
        self.live.send(ServerMessage::ApprovalRequest {
            id: call.id.clone(),
            tool: call.name.clone(),
            command: rhema_cloud_agent::wire::approval_command(call),
        });
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(answer)) => Some(answer),
            // Timed out, or the client left: denied either way.
            _ => {
                self.live.send(ServerMessage::ApprovalExpired {
                    id: call.id.clone(),
                });
                None
            }
        }
    }
}

/// The session key this server persists one conversation under — the
/// same shape the DO's storage uses.
fn session_key(session_id: &str) -> ResourceKey {
    ResourceKey::new(format!("coding-agent/{session_id}"))
}

/// Milliseconds since the Unix epoch, from the host clock.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// The routes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/session/{id}", get(snapshot))
        .route("/config/{id}", post(set_config))
        .route("/ws/{id}", get(chat_socket))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

/// `GET /session/:id`: the checkpointed record, as the client stored
/// it.
async fn snapshot(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let key = session_key(&id);
    match state.store.load(&key) {
        Ok(Some(record)) => Json(serde_json::json!({
            "revision": record.revision,
            "record": serde_json::to_value(&record)
                .map_err(|err| err.to_string())
                .unwrap_or(serde_json::Value::Null),
        }))
        .into_response(),
        Ok(None) => Json(serde_json::json!({ "revision": null })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /config/:id`: installs the session's [`SessionConfig`].
async fn set_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(config): Json<SessionConfig>,
) -> impl IntoResponse {
    let value = match serde_json::to_string(&config) {
        Ok(value) => value,
        Err(err) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("the config does not serialize: {err}")
                })),
            )
                .into_response();
        }
    };
    match state.store.set_meta(&format!("config:{id}"), &value) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// The session's stored config, defaults filling what was never set. A
/// store failure or a row that no longer parses is an error, not a
/// silent default: a turn must not run against a sandbox backend,
/// model, or skill set the client never chose.
fn load_config(state: &AppState, id: &str) -> Result<SessionConfig, String> {
    let key = format!("config:{id}");
    match state.store.meta(&key) {
        Ok(Some(value)) => serde_json::from_str(&value)
            .map_err(|err| format!("the stored config {key:?} no longer parses: {err}")),
        Ok(None) => Ok(SessionConfig::default()),
        Err(err) => Err(format!("the stored config {key:?} is unreadable: {err}")),
    }
}

/// `GET /ws/:id`: accept the socket and hand it to the pump.
async fn chat_socket(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |socket| pump(state, id, socket))
}

/// The socket pump: client messages in, turns spawned, approvals
/// routed. Runs for the connection's life.
async fn pump(state: Arc<AppState>, session_id: String, mut socket: axum::extract::ws::WebSocket) {
    // One connection per session. A second socket for a live session
    // gets the refusal and the close.
    let claimed = state
        .connected
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id.clone());
    if !claimed {
        let _ = socket
            .send(axum::extract::ws::Message::Text(
                serde_json::to_string(&ServerMessage::Error {
                    message: "this session already has a connection".into(),
                })
                .unwrap_or_default()
                .into(),
            ))
            .await;
        return;
    }
    use futures_util::SinkExt as _;
    let (outgoing, mut incoming) = mpsc::unbounded_channel::<ServerMessage>();
    let live = Arc::new(Live {
        outgoing,
        pending: Mutex::new(HashMap::new()),
        cancelled: Arc::new(AtomicBool::new(false)),
        busy: AtomicBool::new(false),
    });
    let channel = Arc::new(LiveChannel {
        live: live.clone(),
        timeout: APPROVAL_TIMEOUT,
    });

    let (mut sink, mut stream) = socket.split();
    // The writer: protocol messages out, one task owning the sink.
    let writer = tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                break;
            };
            if sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    use futures_util::StreamExt;
    // The config loads only after the writer task exists, so a failure
    // reaches the client through it: a stored config that cannot be
    // read or parsed refuses the socket instead of running a turn on
    // defaults the client never chose.
    let config = match load_config(&state, &session_id) {
        Ok(config) => config,
        Err(message) => {
            live.send(ServerMessage::Error { message });
            return;
        }
    };
    while let Some(Ok(frame)) = stream.next().await {
        let axum::extract::ws::Message::Text(text) = frame else {
            continue;
        };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::Chat { text }) => {
                // One turn at a time.
                if live.busy.swap(true, Ordering::SeqCst) {
                    live.send(ServerMessage::Error {
                        message: "a turn is already running".into(),
                    });
                    continue;
                }
                live.cancelled.store(false, Ordering::SeqCst);
                let live_task = live.clone();
                let channel_task = channel.clone();
                let state_task = state.clone();
                let config_task = config.clone();
                let session_task = session_id.clone();
                tokio::spawn(run_turn(
                    live_task,
                    channel_task,
                    state_task,
                    config_task,
                    session_task,
                    text,
                ));
            }
            Ok(ClientMessage::Approve { id, approve }) => {
                if let Some(sender) = live
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = sender.send(approve);
                }
            }
            Ok(ClientMessage::Stop) => {
                live.cancelled.store(true, Ordering::SeqCst);
            }
            Err(err) => {
                live.send(ServerMessage::Error {
                    message: format!("unrecognized message: {err}"),
                });
            }
        }
    }
    // The client is gone: the turn unwinds at its next check, and
    // pending approvals deny by absence.
    live.cancelled.store(true, Ordering::SeqCst);
    live.pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    writer.abort();
    state
        .connected
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id);
}

/// One turn: assemble the agent from durable state, run it,
/// checkpoint around it. The DO's `run_turn`, with tokio where the
/// isolate had one thread.
async fn run_turn(
    live: Arc<Live>,
    channel: Arc<LiveChannel>,
    state: Arc<AppState>,
    config: SessionConfig,
    session_id: String,
    input: String,
) {
    let key = session_key(&session_id);
    // 1. The checkpointed session, if any — the engine's revive
    //    machinery closes committed-but-unanswered calls on resume.
    let (revision, session) = match state.store.load(&key) {
        Ok(Some(record)) => (
            record.revision,
            record
                .session
                .map(|wrapped| wrapped.core)
                .unwrap_or_default(),
        ),
        Ok(None) => (0, Session::default()),
        Err(err) => {
            live.busy.store(false, Ordering::SeqCst);
            live.send(ServerMessage::Error {
                message: format!("checkpoint unreadable: {err}"),
            });
            return;
        }
    };

    // 2. The provider: the Anthropic dialect over reqwest, with the
    //    retry ladder. The short one on both classes: a hosted
    //    function's request outlives no five-minute outage.
    let provider: Arc<dyn ModelProvider> = Arc::new(Retrying::with_ladders(
        Arc::new(AnthropicProvider::new_with_transport(
            state.anthropic_key.clone(),
            Arc::new(ReqwestTransport::new()),
        )),
        vec![Duration::from_secs(1), Duration::from_secs(2)],
        vec![Duration::from_secs(1), Duration::from_secs(2)],
    ));

    // 3. The sandbox of the same session name, seeded on first use.
    //    The backend is the deployment's choice, resolved once at
    //    startup; the session name and workspace are this turn's.
    let sandbox = state.sandbox.for_session(&session_id, &config.workspace);
    if let Err(message) = ensure_seeded(&state.store, sandbox.as_ref(), &config).await {
        live.busy.store(false, Ordering::SeqCst);
        live.send(ServerMessage::Error { message });
        return;
    }

    // 4. The agent: the ordinary assembly, resumed from the checkpoint.
    let assembly = AgentAssembly {
        model: config.model.clone(),
        base_system: config.base_system.clone(),
        skills: config.skills(),
        max_turn_steps: config.max_turn_steps,
        thinking: rhema_core::request::ThinkingEffort::Default,
        workspace: config.workspace.clone(),
        transcript: Some(rhema_transcript::TranscriptSettings::default()),
        approval: config.approval.clone(),
        channel: Some(channel as Arc<dyn rhema_cloud_agent::ApprovalChannel>),
    };
    let shell = Arc::new(rhema_cloud_agent::ShellSandbox::new(sandbox));
    let mut agent = match assembly.assemble_from_session(provider, shell, session) {
        Ok(agent) => agent,
        Err(err) => {
            live.busy.store(false, Ordering::SeqCst);
            live.send(ServerMessage::Error {
                message: err.to_string(),
            });
            return;
        }
    };

    // 5. The commit hook: the checkpoint before every side effect. The
    //    revision rides a cell the hook and this function share.
    let revision = Arc::new(Mutex::new(revision));
    let hook = {
        let store = state.store.clone();
        let key = key.clone();
        let revision = revision.clone();
        Box::new(move |session: &Session| {
            checkpoint(&store, &key, &revision, session).map_err(CommitError)
        }) as rhema_core::CommitHook
    };
    agent.set_commit_hook(Some(hook));

    // 6. The turn. The turn-level approval stays deny: the gate in
    //    front of the shell is the only path a call takes, and the
    //    fail-safe default stands behind it.
    let mut sink = LiveSink { live: live.clone() };
    let result = agent
        .turn_with_cancel(input, &mut sink, Arc::new(DenyAll), live.cancelled.clone())
        .await;

    // 7. The post-turn checkpoint and the close.
    match result {
        Ok(output) => {
            if let Err(message) = checkpoint(&state.store, &key, &revision, agent.session()) {
                live.send(ServerMessage::Error {
                    message: format!("post-turn checkpoint failed: {message}"),
                });
            }
            live.send(ServerMessage::turn_complete(
                output.stop_reason,
                output.usage,
                agent.session().cost_usd,
            ));
        }
        Err(err) => {
            // The history is whole; checkpoint so a follow-up continues
            // instead of replaying.
            let _ = checkpoint(&state.store, &key, &revision, agent.session());
            live.send(ServerMessage::turn_failed(err.to_string()));
        }
    }
    live.busy.store(false, Ordering::SeqCst);
}

/// Seeds the workspace the first time it is used — a git clone when
/// the session carries one, then the skill bodies — and records that
/// in the meta table, so a resumed server does not re-clone over a
/// workspace the client has since changed. The git+skills half is the
/// shared `seed_workspace`; the marker is this server's alone, because
/// it lives in Postgres.
async fn ensure_seeded(
    store: &PgStore,
    sandbox: &dyn Sandbox,
    config: &SessionConfig,
) -> Result<(), String> {
    match store.meta("workspace:seeded") {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(err) => return Err(err.to_string()),
    }
    let skills: Vec<(&str, &str)> = config
        .skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.body.as_str()))
        .collect();
    rhema_sandbox::seed::seed_workspace(
        sandbox,
        config.seed_git.as_deref(),
        &config.workspace,
        &skills,
    )
    .await
    .map_err(|err| err.to_string())?;
    store
        .set_meta("workspace:seeded", "1")
        .map_err(|err| err.to_string())
}

/// Persists `session` under the key at `current + 1`. Called from the
/// commit hook (sync, inside the turn) and after the turn.
fn checkpoint(
    store: &PgStore,
    key: &ResourceKey,
    current: &Mutex<u64>,
    session: &Session,
) -> Result<(), String> {
    let mut current = current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut record = rhema_rsd::KeyRecord::fresh(key.clone(), now_millis());
    record.revision = *current + 1;
    let mut wrapped = rhema_rsd::Session::fresh(key.clone(), now_millis());
    wrapped.core = session.clone();
    record.session = Some(wrapped);
    store
        .save(record, Some(*current))
        .map_err(|err| err.to_string())?;
    *current += 1;
    Ok(())
}
