//! The workers-rs glue — the one module where the platform lives.
//!
//! [`CodingAgent`] is one Durable Object per session id. Its
//! WebSocket surface speaks a small JSON protocol (chat, approval
//! answers, stop; streaming events out), its HTTP surface serves the
//! checkpoint introspection, and every turn is the ordinary coding
//! agent over the ordinary engine: the Anthropic adapter on
//! [`FetchTransport`], `bash` in the session's sandbox — the shim DO
//! ([`SandboxDo`]) or the `http` daemon the deployment selects —
//! checkpoints through [`DoStore`]. Approval asks and their answers
//! cross between the pump and the turn task only through channels —
//! workerd runs everything on one thread.
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
mod sandbox_http;
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

use rhema_agent_tools::DenyAll;
use rhema_core::{Event, EventSink, ModelProvider, Session};
use rhema_provider::Retrying;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_rsd::{ResourceKey, Store, StoreError};

use crate::backend::{BACKEND_VAR, SandboxBackend, SandboxEnv, URL_VAR, select_backend};
use rhema_cloud_agent::{
    AgentAssembly, ClientMessage, Sandbox, SandboxError, ServerMessage, SessionConfig,
    ShellSandbox, WorkspaceSeed,
};
use sandbox::SandboxSource;
use storage::session_key;

/// How long an approval ask waits for its client before the gate
/// denies. The client sees `approval_expired` when it happens.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The streaming sink: every engine event serialized to the socket.
struct WsSink {
    live: Arc<Live>,
}

impl EventSink for WsSink {
    fn emit(&mut self, event: Event<'_>) {
        self.live.send(&ServerMessage::from_event(event));
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
/// constructor. `missing_docs` is expected to fire here and only here:
/// the macro generates the wasm-bindgen constructor and fetch wrapper,
/// which no doc comment can reach, and this module holds nothing else.
/// The sanctioned `unsafe` exception (the macro emits some) is scoped
/// to this module too.
#[expect(
    missing_docs,
    reason = "the durable_object macro generates the \
    wasm-bindgen constructor and fetch wrapper, which no doc comment can reach"
)]
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

/// One plain var of the deployment env, absent when unset — the
/// `RHEMA_MODEL` pattern; secrets go through `env.secret`.
fn env_var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
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
    // The sandbox backend rides the deployment env (spec: backend
    // selection rides deployment env), read once per connection here
    // where every other piece of env is. Under `http` the shim's DO
    // binding is never resolved — no namespace call happens — so the
    // binding is absent from deployments that do not need it.
    let sandbox = match select_backend(&SandboxEnv {
        backend: env_var(env, BACKEND_VAR),
        url: env_var(env, URL_VAR),
    })
    .map_err(|err| Error::from(err.to_string()))?
    {
        SandboxBackend::Cloudflare => SandboxSource::Cloudflare(env.durable_object("SANDBOX")?),
        SandboxBackend::Http(base) => SandboxSource::Http(base),
    };

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
        sandbox,
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
    sandbox: SandboxSource,
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
                            live.send(&ServerMessage::Error {
                                message: "a turn is already running".into(),
                            });
                            continue;
                        }
                        live.cancelled.store(false, Ordering::SeqCst);
                        wasm_bindgen_futures::spawn_local(run_turn(
                            live.clone(),
                            channel.clone(),
                            store.clone(),
                            config.clone(),
                            api_key.clone(),
                            sandbox.clone(),
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
                        live.send(&ServerMessage::Error {
                            message: format!("unrecognized message: {err}"),
                        });
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
#[expect(
    clippy::too_many_arguments,
    reason = "the turn's inputs are the DO's \
    durable pieces, each from a different source; a struct would only move the list"
)]
async fn run_turn(
    live: Arc<Live>,
    channel: Arc<WsChannel>,
    store: DoStore,
    config: SessionConfig,
    api_key: String,
    sandbox: SandboxSource,
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
            live.send(&ServerMessage::Error { message });
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

    // 3. The sandbox of the same session name, seeded on first use —
    //    the shim DO wrapper or the shared HTTP client, per the
    //    backend the deployment selected.
    let sandbox = sandbox.for_session(&session_id, &sandbox_token);
    if let Err(err) = ensure_seeded(&store, sandbox.as_ref(), &config).await {
        live.busy.store(false, Ordering::SeqCst);
        live.send(&ServerMessage::Error {
            message: err.to_string(),
        });
        return;
    }

    // 4. The agent: the ordinary assembly, resumed from the checkpoint.
    let skills = config.skills();
    let assembly = AgentAssembly {
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
            live.send(&ServerMessage::Error {
                message: err.to_string(),
            });
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

    // 7. The post-turn checkpoint and the close. A failed checkpoint is
    //    reported to the client AND logged: the reply still leaves, but
    //    the history only survives an eviction if the write did.
    match result {
        Ok(output) => {
            if let Err(err) = checkpoint(&store, &key, &revision, agent.session()) {
                worker::console_error!("checkpoint after a completed turn failed for {key}: {err}");
                live.send(&ServerMessage::Error {
                    message: format!("post-turn checkpoint failed: {err}"),
                });
            }
            live.send(&ServerMessage::turn_complete(
                output.stop_reason,
                output.usage,
                agent.session().cost_usd,
            ));
        }
        Err(err) => {
            // The history is whole; checkpoint so a follow-up continues
            // instead of replaying. Losing this write to a failed save
            // is logged loudly: the next session would start from an
            // older revision with no other signal.
            if let Err(state) = checkpoint(&store, &key, &revision, agent.session()) {
                worker::console_error!("checkpoint on a failed turn lost for {key}: {state}");
            }
            live.send(&ServerMessage::turn_failed(err.to_string()));
        }
    }
    live.busy.store(false, Ordering::SeqCst);
}

/// Why making a turn's durable pieces failed: the DO's storage refused
/// a checkpoint or meta write, or the sandbox refused the workspace
/// seed. One type so every turn-loop failure reports the same way.
#[derive(Debug, thiserror::Error)]
pub enum TurnStateError {
    /// The DO's SQLite storage layer refused a read or write.
    #[error("session store: {0}")]
    Store(#[from] StoreError),
    /// The sandbox refused the workspace seed (a git clone that would
    /// not clone, a sandbox that is not up).
    #[error("workspace seed: {0}")]
    Seed(#[from] SandboxError),
    /// One skill's `SKILL.md` write failed; the name says which.
    #[error("seeding skill {name}: {source}")]
    Skill {
        /// The skill whose body could not be written.
        name: String,
        /// The sandbox's refusal.
        source: SandboxError,
    },
}

/// Seeds the workspace the first time it is used — a git clone when the
/// session carries one — and records that in the meta table, so a
/// resumed DO does not re-clone over a workspace the client has since
/// changed. A full container eviction loses the files; the next
/// session re-seeds by clearing the marker (SPEC.md).
async fn ensure_seeded(
    store: &DoStore,
    sandbox: &dyn Sandbox,
    config: &SessionConfig,
) -> Result<(), TurnStateError> {
    match store.meta("workspace:seeded") {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(err) => return Err(err.into()),
    }
    if let Some(url) = &config.seed_git {
        sandbox
            .seed(&WorkspaceSeed::GitClone(url.clone()))
            .await
            .map_err(TurnStateError::from)?;
    }
    for skill in &config.skills {
        let path = format!("{}/.rhema/skills/{}/SKILL.md", config.workspace, skill.name);
        sandbox
            .write_file(&path, skill.body.as_bytes())
            .await
            .map_err(|err| TurnStateError::Skill {
                name: skill.name.clone(),
                source: err,
            })?;
    }
    store
        .set_meta("workspace:seeded", "1")
        .map_err(TurnStateError::from)
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
) -> Result<(), TurnStateError> {
    let mut current = current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut record = storage::fresh_record(key.clone());
    record.revision = *current + 1;
    let mut wrapped = rhema_rsd::Session::fresh(key.clone(), storage::now_millis());
    wrapped.core = session.clone();
    record.session = Some(wrapped);
    store.save(record, Some(*current))?;
    *current += 1;
    Ok(())
}
