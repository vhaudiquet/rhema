//! The workers-rs glue — the one module where the platform lives.
//!
//! [`CodingAgent`] is one Durable Object per session id. Its
//! WebSocket surface speaks a small JSON protocol (chat, approval
//! answers, stop; streaming events out), its HTTP surface serves the
//! checkpoint introspection, and every turn is the ordinary coding
//! agent over the ordinary engine: the Anthropic adapter on
//! [`FetchTransport`], `bash` in the session's sandbox
//! ([`SandboxDo`]), checkpoints through [`DoStore`]. Approval asks and
//! their answers cross between the pump and the turn task only through
//! channels — workerd runs everything on one thread.
//!
//! Turn driving (v1): a turn runs inside the client's live connection.
//! Checkpoints land at the commit hook — before every side effect —
//! and after each turn, so an eviction resumes through the engine's
//! revive machinery; the alarm-driven queue that would let turns
//! outlive requests is a v2 refinement (SPEC.md).
//!
//! Layout: the [`do_class`] submodule holds the `#[durable_object]`
//! macro invocation — the one place the sanctioned `unsafe` exception
//! applies, and the one place `missing_docs` is off, because the macro
//! generates the wasm-bindgen constructor no doc comment can reach.
//! Everything hand-written lives outside it, under the crate's lints.
#![forbid(unsafe_code)]

mod approval;
mod sandbox;
mod sleeper;
mod storage;
mod transport;

pub use approval::{Live, WsChannel};
pub use sandbox::SandboxDo;
pub use sleeper::DelaySleeper;
pub use storage::DoStore;
pub use transport::FetchTransport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use worker::{Env, Error, Method, Request, Response, Result, State, WebSocketPair};

use rhema_agent_tools::{DenyAll, Skill};
use rhema_core::{Event, EventSink, ModelProvider, Session};
use rhema_provider::Retrying;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_rsd::{ResourceKey, Store};

use crate::assembly::ApprovalPolicy;
use crate::sandbox::{Sandbox, WorkspaceSeed};
use crate::shell::ShellSandbox;
use storage::session_key;

/// How long an approval ask waits for its client before the gate
/// denies. The client sees `approval_expired` when it happens.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The default model, when the session carries no config yet.
const DEFAULT_MODEL: &str = "claude-sonnet-4-5";

/// The sandbox workspace path.
const DEFAULT_WORKSPACE: &str = "/workspace";

/// One skill seeded into the sandbox and rendered into the prompt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillSeed {
    /// The skill's folder name.
    pub name: String,
    /// The frontmatter description — the trigger.
    pub description: String,
    /// The whole `SKILL.md`, frontmatter included.
    pub body: String,
}

/// The per-session configuration, persisted in the DO's meta table and
/// set by a client over `POST /config`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionConfig {
    /// Model identifier, passed through to the provider.
    pub model: String,
    /// The session's SYSTEM.md, stored verbatim; `None` renders the
    /// built-in default.
    #[serde(default)]
    pub base_system: Option<String>,
    /// The approval policy gating `bash`.
    #[serde(default)]
    pub approval: ApprovalPolicy,
    /// The workspace path inside the sandbox.
    #[serde(default = "default_workspace")]
    pub workspace: String,
    /// A git URL to clone when the workspace is first (re)seeded.
    #[serde(default)]
    pub seed_git: Option<String>,
    /// Skills seeded into the sandbox and rendered into the prompt.
    #[serde(default)]
    pub skills: Vec<SkillSeed>,
    /// Cap on steps per turn.
    #[serde(default = "default_max_turn_steps")]
    pub max_turn_steps: u32,
}

fn default_workspace() -> String {
    DEFAULT_WORKSPACE.to_owned()
}

fn default_max_turn_steps() -> u32 {
    128
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            base_system: None,
            approval: ApprovalPolicy::default(),
            workspace: default_workspace(),
            seed_git: None,
            skills: Vec::new(),
            max_turn_steps: default_max_turn_steps(),
        }
    }
}

/// What a connected client sends.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Start one turn with this user text.
    Chat { text: String },
    /// Answer a pending approval.
    Approve { id: String, approve: bool },
    /// Ask the running turn to stop.
    Stop,
}

/// The streaming sink: every engine event serialized to the socket.
struct WsSink {
    live: Arc<Live>,
}

impl EventSink for WsSink {
    fn emit(&mut self, event: Event<'_>) {
        self.live.send_json(&event_json(event));
    }
}

/// One engine event as the protocol carries it.
fn event_json(event: Event<'_>) -> serde_json::Value {
    match event {
        Event::StepStart { iteration } => {
            serde_json::json!({ "type": "step_start", "iteration": iteration })
        }
        Event::ThinkingDelta(delta) => {
            serde_json::json!({ "type": "thinking_delta", "delta": delta })
        }
        Event::TextDelta(delta) => serde_json::json!({ "type": "text_delta", "delta": delta }),
        Event::ToolCallStart { id, name } => {
            serde_json::json!({ "type": "tool_call_start", "id": id, "name": name })
        }
        Event::ToolCallArgsDelta { id, delta } => {
            serde_json::json!({ "type": "tool_call_args_delta", "id": id, "delta": delta })
        }
        Event::ToolOutputDelta { id, chunk } => {
            serde_json::json!({ "type": "tool_output_delta", "id": id, "chunk": chunk })
        }
        Event::ToolResult {
            id,
            content,
            is_error,
            code,
        } => serde_json::json!({
            "type": "tool_result", "id": id, "content": content,
            "is_error": is_error, "code": code,
        }),
        Event::StepEnd { usage, stop_reason } => {
            serde_json::json!({ "type": "step_end", "usage": usage, "stop_reason": stop_reason })
        }
    }
}

/// The routes of the DO. Free-standing so the hand-written logic keeps
/// the crate's documentation lint; only the class module below is
/// exempt from it.
async fn route(state: &State, env: &Env, req: Request) -> Result<Response> {
    let path = req.path();
    match path.as_str() {
        "/healthz" => Response::from_json(&serde_json::json!({ "ok": true })),
        "/session" => snapshot(state).await,
        "/config" if req.method() == Method::Post => set_config(req, state).await,
        "/ws" => {
            let upgrade = req
                .headers()
                .get("Upgrade")?
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
            if !upgrade {
                return Response::error("the /ws route upgrades to a WebSocket", 426);
            }
            chat_socket(state, env).await
        }
        _ => Response::error("not found", 404),
    }
}

/// The class the `#[durable_object]` macro turns into a JS
/// constructor. `missing_docs` is off here and only here: the macro
/// generates the wasm-bindgen constructor and fetch wrapper, which no
/// doc comment can reach, and this module holds nothing else. The
/// sanctioned `unsafe` exception (the macro emits some) is scoped to
/// this module too.
#[allow(missing_docs)]
mod do_class {
    use super::route;
    use worker::wasm_bindgen;
    use worker::{DurableObject, Env, Request, Response, Result, State, durable_object};

    #[durable_object]
    pub struct CodingAgent {
        pub(crate) state: State,
        pub(crate) env: Env,
    }

    impl DurableObject for CodingAgent {
        fn new(state: State, env: Env) -> Self {
            Self { state, env }
        }

        async fn fetch(&self, req: Request) -> Result<Response> {
            route(&self.state, &self.env, req).await
        }
    }
}
pub use do_class::CodingAgent;

/// The store over this DO's database.
fn store_of(state: &State) -> Result<DoStore> {
    DoStore::new(state.storage().sql()).map_err(|err| Error::from(err.to_string()))
}

/// GET /session: the checkpointed record, as the client stored it.
async fn snapshot(state: &State) -> Result<Response> {
    let store = store_of(state)?;
    let key = session_key(&state.id().to_string());
    let record = store
        .load_checked(&key)
        .map_err(|err| Error::from(err.to_string()))?;
    match record {
        Some(record) => Response::from_json(&serde_json::json!({
            "revision": record.revision,
            "record": serde_json::to_value(&record)
                .map_err(|err| Error::from(err.to_string()))?,
        })),
        None => Response::from_json(&serde_json::json!({ "revision": null })),
    }
}

/// POST /config: installs the session's [`SessionConfig`].
async fn set_config(mut req: Request, state: &State) -> Result<Response> {
    let config: SessionConfig = req.json().await?;
    let store = store_of(state)?;
    let value = serde_json::to_string(&config).map_err(|err| Error::from(err.to_string()))?;
    store
        .set_meta("config", &value)
        .map_err(|err| Error::from(err.to_string()))?;
    Response::from_json(&serde_json::json!({ "ok": true }))
}

/// The stored config, defaults filling what was never set.
fn load_config(env: &Env, store: &DoStore) -> Result<SessionConfig> {
    let stored = store
        .meta("config")
        .map_err(|err| Error::from(err.to_string()))?;
    match stored {
        Some(value) => serde_json::from_str(&value)
            .map_err(|err| Error::from(format!("stored config is malformed: {err}"))),
        None => {
            let mut config = SessionConfig::default();
            if let Ok(model) = env.var("RHEMA_MODEL") {
                config.model = model.to_string();
            }
            Ok(config)
        }
    }
}

/// GET /ws: accept the socket and hand it to the pump. The pump and
/// the turns it spawns run for the life of the connection — the
/// client's live request.
async fn chat_socket(state: &State, env: &Env) -> Result<Response> {
    let pair = WebSocketPair::new()?;
    state.accept_web_socket(&pair.server);

    let store = store_of(state)?;
    let config = load_config(env, &store)?;
    let session_id = state.id().to_string();
    // Secrets ride wrangler secrets, never DO storage.
    let api_key = env
        .secret("ANTHROPIC_API_KEY")
        .map_err(|_| Error::from("the ANTHROPIC_API_KEY secret is not set"))?
        .to_string();
    let sandbox_token = env
        .secret("SANDBOX_TOKEN")
        .map(|secret| secret.to_string())
        .unwrap_or_default();
    let namespace = env.durable_object("SANDBOX")?;

    let live = Arc::new(Live {
        socket: pair.server.clone(),
        pending: Mutex::new(HashMap::new()),
        cancelled: Arc::new(AtomicBool::new(false)),
        busy: AtomicBool::new(false),
    });
    let channel = Arc::new(WsChannel::new(live.clone(), APPROVAL_TIMEOUT));

    wasm_bindgen_futures::spawn_local(pump(
        pair.server,
        live,
        channel,
        store,
        config,
        api_key,
        namespace,
        sandbox_token,
        session_id,
    ));
    Response::from_websocket(pair.client)
}

/// The socket pump: client messages in, turns spawned, approvals
/// routed. Runs for the connection's life.
async fn pump(
    server: worker::WebSocket,
    live: Arc<Live>,
    channel: Arc<WsChannel>,
    store: DoStore,
    config: SessionConfig,
    api_key: String,
    namespace: worker::ObjectNamespace,
    sandbox_token: String,
    session_id: String,
) {
    let mut events = match server.events() {
        Ok(events) => events,
        Err(_) => return,
    };
    while let Some(Ok(event)) = events.next().await {
        match event {
            worker::WebsocketEvent::Message(message) => {
                let Some(text) = message.text() else {
                    continue;
                };
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Chat { text }) => {
                        // One turn at a time.
                        if live.busy.swap(true, Ordering::SeqCst) {
                            live.send_json(&serde_json::json!({
                                "type": "error",
                                "message": "a turn is already running",
                            }));
                            continue;
                        }
                        live.cancelled.store(false, Ordering::SeqCst);
                        wasm_bindgen_futures::spawn_local(run_turn(
                            live.clone(),
                            channel.clone(),
                            store.clone(),
                            config.clone(),
                            api_key.clone(),
                            namespace.clone(),
                            sandbox_token.clone(),
                            session_id.clone(),
                            text,
                        ));
                    }
                    Ok(ClientMessage::Approve { id, approve }) => {
                        channel.answer(&id, approve);
                    }
                    Ok(ClientMessage::Stop) => {
                        live.cancelled.store(true, Ordering::SeqCst);
                    }
                    Err(err) => {
                        live.send_json(&serde_json::json!({
                            "type": "error",
                            "message": format!("unrecognized message: {err}"),
                        }));
                    }
                }
            }
            worker::WebsocketEvent::Close(_) => {
                // The client is gone: the turn unwinds at its next
                // check, and pending approvals deny by absence.
                live.cancelled.store(true, Ordering::SeqCst);
                channel.abandon_all();
                break;
            }
        }
    }
}

/// One turn: assemble the agent from durable state, run it, checkpoint
/// around it. Every piece comes from the DO's storage or its bindings;
/// nothing reads a filesystem.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    live: Arc<Live>,
    channel: Arc<WsChannel>,
    store: DoStore,
    config: SessionConfig,
    api_key: String,
    namespace: worker::ObjectNamespace,
    sandbox_token: String,
    session_id: String,
    input: String,
) {
    let key = session_key(&session_id);
    // 1. The checkpointed session, if any — the engine's revive
    //    machinery closes committed-but-unanswered calls on resume.
    let checkpointed = store
        .load_checked(&key)
        .map_err(|err| format!("checkpoint unreadable: {err}"));
    let (revision, session) = match checkpointed {
        Ok(Some(record)) => (
            record.revision,
            record
                .session
                .map(|wrapped| wrapped.core)
                .unwrap_or_default(),
        ),
        Ok(None) => (0, Session::default()),
        Err(message) => {
            live.busy.store(false, Ordering::SeqCst);
            live.send_json(&serde_json::json!({ "type": "error", "message": message }));
            return;
        }
    };

    // 2. The provider: the Anthropic dialect over host fetch, with the
    //    retry ladder paused by workerd's timers.
    let provider: Arc<dyn ModelProvider> = Arc::new(Retrying::policy_with_sleeper(
        Arc::new(AnthropicProvider::new_with_transport(
            api_key,
            Arc::new(FetchTransport::new()),
        )),
        3,
        Duration::from_secs(1),
        Duration::from_secs(2),
        Arc::new(DelaySleeper),
    ));

    // 3. The sandbox of the same session name, seeded on first use.
    let sandbox = Arc::new(SandboxDo::new(namespace, session_id.clone(), sandbox_token));
    if let Err(message) = ensure_seeded(&store, sandbox.as_ref(), &config).await {
        live.busy.store(false, Ordering::SeqCst);
        live.send_json(&serde_json::json!({ "type": "error", "message": message }));
        return;
    }

    // 4. The agent: the ordinary assembly, resumed from the checkpoint.
    let skills = seeded_skills(&config, &config.workspace);
    let assembly = crate::assembly::AgentAssembly {
        model: config.model.clone(),
        base_system: config.base_system.clone(),
        skills,
        max_turn_steps: config.max_turn_steps,
        thinking: rhema_core::request::ThinkingEffort::Default,
        workspace: config.workspace.clone(),
        transcript: Some(rhema_transcript::TranscriptSettings::default()),
        approval: config.approval.clone(),
        channel: Some(channel),
    };
    let shell = Arc::new(ShellSandbox::new(sandbox));
    let mut agent = match assembly.assemble_from_session(provider, shell, session) {
        Ok(agent) => agent,
        Err(err) => {
            live.busy.store(false, Ordering::SeqCst);
            live.send_json(&serde_json::json!({ "type": "error", "message": err.to_string() }));
            return;
        }
    };

    // 5. The commit hook: the checkpoint before every side effect. The
    //    revision rides a cell the hook and this function share.
    let revision = Arc::new(Mutex::new(revision));
    let hook = {
        let store = store.clone();
        let key = key.clone();
        let revision = revision.clone();
        Box::new(move |session: &Session| {
            checkpoint(&store, &key, &revision, session).map_err(|err| err.to_string())
        }) as rhema_core::CommitHook
    };
    agent.set_commit_hook(Some(hook));

    // 6. The turn. The turn-level approval stays deny: the gate in
    //    front of the shell is the only path a call takes, and the
    //    fail-safe default stands behind it.
    let mut sink = WsSink { live: live.clone() };
    let result = agent
        .turn_with_cancel(input, &mut sink, Arc::new(DenyAll), live.cancelled.clone())
        .await;

    // 7. The post-turn checkpoint and the close.
    match result {
        Ok(output) => {
            if let Err(message) = checkpoint(&store, &key, &revision, agent.session()) {
                live.send_json(&serde_json::json!({
                    "type": "error",
                    "message": format!("post-turn checkpoint failed: {message}"),
                }));
            }
            live.send_json(&serde_json::json!({
                "type": "turn_complete",
                "stop_reason": output.stop_reason,
                "usage": output.usage,
                "cost_usd": agent.session().cost_usd,
            }));
        }
        Err(err) => {
            // The history is whole; checkpoint so a follow-up continues
            // instead of replaying.
            let _ = checkpoint(&store, &key, &revision, agent.session());
            live.send_json(&serde_json::json!({
                "type": "turn_complete",
                "error": err.to_string(),
            }));
        }
    }
    live.busy.store(false, Ordering::SeqCst);
}

/// Seeds the workspace the first time it is used — a git clone when the
/// session carries one — and records that in the meta table, so a
/// resumed DO does not re-clone over a workspace the client has since
/// changed. A full container eviction loses the files; the next
/// session re-seeds by clearing the marker (SPEC.md).
async fn ensure_seeded(
    store: &DoStore,
    sandbox: &SandboxDo,
    config: &SessionConfig,
) -> Result<(), String> {
    match store.meta("workspace:seeded") {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(err) => return Err(err.to_string()),
    }
    if let Some(url) = &config.seed_git {
        sandbox
            .seed(&WorkspaceSeed::GitClone(url.clone()))
            .await
            .map_err(|err| err.to_string())?;
    }
    for skill in &config.skills {
        let path = format!("{}/.rhema/skills/{}/SKILL.md", config.workspace, skill.name);
        sandbox
            .write_file(&path, skill.body.as_bytes())
            .await
            .map_err(|err| format!("seeding skill {}: {err}", skill.name))?;
    }
    store
        .set_meta("workspace:seeded", "1")
        .map_err(|err| err.to_string())
}

/// The skill list the prompt renders, against the paths the files were
/// seeded at — the model reads the bodies through the shell.
fn seeded_skills(config: &SessionConfig, workspace: &str) -> Vec<Skill> {
    config
        .skills
        .iter()
        .map(|seed| Skill {
            name: seed.name.clone(),
            description: seed.description.clone(),
            path: format!("{workspace}/.rhema/skills/{}/SKILL.md", seed.name).into(),
        })
        .collect()
}

/// Persists `session` under the key at `current + 1`. Called from the
/// commit hook (sync, inside the turn) and after the turn. The engine's
/// session rides in the record's RSD wrapper — the same shape the
/// dispatcher checkpoints — so the row stays one contract's shape.
fn checkpoint(
    store: &DoStore,
    key: &ResourceKey,
    current: &Mutex<u64>,
    session: &Session,
) -> Result<(), String> {
    let mut current = current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut record = storage::fresh_record(key.clone());
    record.revision = *current + 1;
    let mut wrapped = rhema_rsd::Session::fresh(key.clone(), storage::now_millis());
    wrapped.core = session.clone();
    record.session = Some(wrapped);
    store
        .save(record, Some(*current))
        .map_err(|err| err.to_string())?;
    *current += 1;
    Ok(())
}
