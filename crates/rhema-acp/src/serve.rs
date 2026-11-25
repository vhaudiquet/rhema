//! The ACP agent surface: one stdio connection, six handlers.
//!
//! Handlers follow the SDK's ordering contract. Everything that answers
//! immediately (initialize, authenticate, session/new, session/load,
//! session/cancel) runs inside the dispatch loop; `session/prompt`,
//! minutes long, spawns its turn task through [`ConnectionTo::spawn`]
//! and returns at once, so `session/cancel` and the client's permission
//! answers keep flowing while the turn runs.
//!
//! Session state lives in `App`: one slot per session holding the
//! [`HostAgent`], a cancel sender, and the `always` permission memory.
//! The prompt task *takes* the agent out for the turn's duration, which
//! makes a second concurrent prompt for the same session a clean error
//! instead of a data race.
//!
//! Reported, beyond the turn itself: the commands a prompt's first
//! line can name (`thinking`, `model`), the two selects a config-aware
//! client renders (`model`, `thinking`), the session's title and last
//! activity, and the context window and cost each step leaves behind,
//! the meter they share also totals every `session/prompt` response.
//!
//! Deliberately absent: MCP servers (`mcpServers` is accepted and
//! ignored, nothing is advertised), fs/terminal callbacks (the `bash`
//! tool does its own I/O), and session modes (nothing advertised, so
//! no client offers them).

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthMethodId, AuthenticateRequest,
    AuthenticateResponse, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, ConfigOptionUpdate, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionId, SessionInfoUpdate,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, UnstructuredCommandInput,
};
use agent_client_protocol::{
    Agent, Client, Error, Stdio, on_receive_notification, on_receive_request,
};
use anyhow::Context;
use rhema_config::Settings;
use rhema_core::request::ThinkingEffort;
use rhema_core::{Content, ModelProvider, Role, Session, StopReason as EngineStop};
use rhema_host_agent::{HostAgent, HostAgentConfig, HostAgentError};
use rhema_session_store::{SessionId as StoreSessionId, SessionStore};
use rhema_session_store_file::FileStore;
use tokio::sync::watch;

use crate::approval::{AcpApproval, AlwaysMemory};
use crate::backend;
use crate::sink::{AcpSink, tool_kind};

mod app;
mod wire;

use wire::*;

pub(crate) use app::{App, SessionSlot, SlotRestore};
/// Everything the `rhema acp` flags contribute; the rest comes from the
/// stored config, exactly like the headless one-shot.
#[derive(Debug, Clone, Default)]
pub struct AcpOptions {
    /// Which config profile to use; `None` is the default profile.
    pub profile: Option<String>,
    /// Model backend override (a provider name like `google`,
    /// `anthropic`, `openai`, `deepseek`, `zai`, `openrouter`,
    /// `llama-cpp`).
    pub provider: Option<String>,
    /// Model name override.
    pub model: Option<String>,
    /// llama.cpp server base URL override.
    pub llama_url: Option<String>,
    /// Cap on steps per turn; `None` is the host agent's default.
    pub max_steps: Option<NonZeroU32>,
    /// Reasoning effort override; `None` falls back to the profile,
    /// then the provider default.
    pub thinking: Option<ThinkingEffort>,
    /// Where sessions persist; `None` is the XDG cache default. A
    /// deployment (or a test) can redirect the whole directory.
    pub session_dir: Option<std::path::PathBuf>,
    /// A scripted provider standing in for the backend, so tests drive
    /// the full protocol offline. Present, it replaces credential
    /// resolution entirely; the model name is [`Self::model`] or
    /// `"mock"`.
    #[cfg(feature = "mock")]
    pub mock_replies: Option<Vec<rhema_core::ModelOutput>>,
    /// A config replacing the stored one, so tests exercise credential
    /// resolution without reading (or depending on) the user's file.
    #[cfg(feature = "mock")]
    pub config: Option<rhema_config::Settings>,
}

/// Serves one ACP connection over stdin/stdout until the client hangs
/// up. The connection runs on a dedicated thread with its own
/// multi-thread runtime, the approval bridge parks workers with
/// `block_in_place`, which needs that scheduler, and a thread of its
/// own means the caller can sit inside any runtime (the `rhema` binary
/// is `#[tokio::main]`) without nesting one. Blocks the calling thread.
pub fn serve(options: AcpOptions) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rhema-acp".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("cannot start the ACP runtime")?;
            runtime.block_on(serve_connection(Stdio::new(), options))
        })
        .context("cannot spawn the ACP thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("the ACP thread panicked"))?
}

/// Serves one ACP connection over `transport`, [`serve`] minus stdio,
/// the seam an in-memory client (the SDK's [`Channel`](agent_client_protocol::Channel)
/// pair in tests) drives.
pub async fn serve_over(
    transport: impl agent_client_protocol::ConnectTo<Agent>,
    options: AcpOptions,
) -> anyhow::Result<()> {
    serve_connection(transport, options).await
}

async fn serve_connection(
    transport: impl agent_client_protocol::ConnectTo<Agent>,
    options: AcpOptions,
) -> anyhow::Result<()> {
    // Startup reads, the config file, the secret store behind the
    // profile's `secret_ref`s, the user's SYSTEM.md, are blocking,
    // and this async context owns a scheduler: the work hops to the
    // blocking pool instead of parking a worker.
    let app = Arc::new(
        tokio::task::spawn_blocking(move || App::new(options))
            .await
            .context("the ACP app failed to start")?,
    );
    let result = Agent
        .v1()
        .name("rhema")
        .on_receive_request(
            async move |_request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new().load_session(true))
                        .auth_methods(vec![AuthMethod::Agent(
                            AuthMethodAgent::new(
                                AuthMethodId::new("rhema-stored"),
                                "Stored rhema credentials",
                            )
                            .description(
                                "Uses the API keys and profiles stored in the rhema config",
                            ),
                        )])
                        .agent_info(Implementation::new("rhema", env!("CARGO_PKG_VERSION"))),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let app = app.clone();
                async move |_request: AuthenticateRequest, responder, _cx| {
                    match app.backend() {
                        // rhema handles auth itself from stored config;
                        // the call doubles as a credential check.
                        Some(_) => responder.respond(AuthenticateResponse::new()),
                        None => Err(Error::internal_error().data(
                            "no rhema credentials resolved: set RHEMA_PROVIDER \
                             and an API key (e.g. GEMINI_API_KEY), or log in \
                             once with the rhema TUI",
                        )),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let app = app.clone();
                async move |request: NewSessionRequest, responder, cx| {
                    let Some(backend) = app.backend() else {
                        // The code editors check to offer their login
                        // flow before retrying.
                        return Err(Error::auth_required().data(
                            "no rhema credentials resolved: set RHEMA_PROVIDER, \
                             authenticate, or store an API key (e.g. GEMINI_API_KEY) \
                             in the rhema config",
                        ));
                    };
                    let provider = backend.provider.clone();
                    let id = app.fresh_id();
                    let agent = HostAgent::new(app.agent_config(
                        backend.provider,
                        app.thinking(),
                        &backend.model,
                        &request.cwd,
                        &id,
                    ))
                    .map_err(|err| Error::internal_error().data(err.to_string()))?;
                    let catalog = app.catalog().await;
                    let config_options =
                        app.config_selects(backend.kind, &backend.model, app.thinking(), &catalog);
                    let (context_size, pricing) =
                        App::card_facts(&catalog, backend.kind, &backend.model);
                    let slot = SessionSlot {
                        kind: backend.kind,
                        provider,
                        agent: Some(agent),
                        model: backend.model.clone(),
                        workspace: request.cwd.clone(),
                        catalog_rows: catalog,
                        thinking: app.thinking(),
                        context_size,
                        pricing,
                        meter: Arc::new(Mutex::new(crate::sink::SessionMeter::default())),
                        title: None,
                        titled: false,
                        cancel: None,
                        last_used: std::time::Instant::now(),
                        memory: Arc::new(Mutex::new(AlwaysMemory::default())),
                    };
                    app.lock().insert(id.clone(), slot);
                    app.evict_idle();
                    // The commands a prompt can name, before the
                    // response: the client renders them as the
                    // session's slash commands.
                    let _ = cx.send_notification(SessionNotification::new(
                        SessionId::new(id.clone()),
                        SessionUpdate::AvailableCommandsUpdate(app.available_commands()),
                    ));
                    responder.respond(
                        NewSessionResponse::new(SessionId::new(id)).config_options(config_options),
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let app = app.clone();
                async move |request: LoadSessionRequest, responder, cx| {
                    let Some(backend) = app.backend() else {
                        return Err(Error::internal_error().data(
                            "no rhema credentials resolved: run authenticate first \
                             or store an API key",
                        ));
                    };
                    let base = app.base.as_ref().ok_or_else(|| {
                        Error::internal_error().data("no cache directory: cannot restore sessions")
                    })?;
                    let id = StoreSessionId::new(&*request.session_id.0).map_err(|err| {
                        Error::invalid_params()
                            .data(format!("session {}: {err}", request.session_id.0))
                    })?;
                    let mut session = FileStore::new(base)
                        .load(&id)
                        .map_err(|err| {
                            Error::invalid_params()
                                .data(format!("session {}: {err}", request.session_id.0))
                        })?
                        .ok_or_else(|| {
                            Error::invalid_params().data(format!(
                                "session {}: no saved session",
                                request.session_id.0
                            ))
                        })?;
                    // A record left `Running` by a dead process is not
                    // running here: ask its kind what the session says,
                    // and write only when something changed, so a plain
                    // reconnect does not rewrite the file.
                    let changes = rhema_submissions::reconcile(
                        &mut session,
                        &app.kinds(),
                        rhema_submissions::now(),
                    )
                    .map_err(|err| {
                        Error::internal_error()
                            .data(format!("session {}: {err}", request.session_id.0))
                    })?;
                    if !changes.is_empty() {
                        FileStore::new(base).save(&id, &session).map_err(|err| {
                            Error::internal_error()
                                .data(format!("session {}: {err}", request.session_id.0))
                        })?;
                    }
                    for change in &changes {
                        if let rhema_submissions::Change::Unclaimed { id, kind } = change {
                            eprintln!(
                                "rhema-acp: session {}: submission {id} belongs to \
                                 the unregistered kind {kind}",
                                request.session_id.0
                            );
                        }
                    }
                    let provider = backend.provider.clone();
                    let agent = HostAgent::from_session(
                        app.agent_config(
                            backend.provider,
                            app.thinking(),
                            &backend.model,
                            &request.cwd,
                            &request.session_id.0,
                        ),
                        session,
                    )
                    .map_err(|err| Error::internal_error().data(err.to_string()))?;
                    replay(&cx, &request.session_id, agent.session());
                    let id = request.session_id.0.to_string();
                    let catalog = app.catalog().await;
                    let config_options =
                        app.config_selects(backend.kind, &backend.model, app.thinking(), &catalog);
                    let (context_size, pricing) =
                        App::card_facts(&catalog, backend.kind, &backend.model);
                    let slot = SessionSlot {
                        kind: backend.kind,
                        provider,
                        agent: Some(agent),
                        model: backend.model.clone(),
                        workspace: request.cwd.clone(),
                        catalog_rows: catalog,
                        thinking: app.thinking(),
                        context_size,
                        pricing,
                        meter: Arc::new(Mutex::new(crate::sink::SessionMeter::default())),
                        title: None,
                        titled: false,
                        cancel: None,
                        last_used: std::time::Instant::now(),
                        memory: Arc::new(Mutex::new(AlwaysMemory::default())),
                    };
                    app.lock().insert(id.clone(), slot);
                    app.evict_idle();
                    let _ = cx.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AvailableCommandsUpdate(app.available_commands()),
                    ));
                    responder.respond(LoadSessionResponse::new().config_options(config_options))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let app = app.clone();
                async move |request: PromptRequest, responder, cx| {
                    // A fresh handle per call: the spawned turn task
                    // needs its own Arc, and an AsyncFnMut closure may
                    // not give up what it captured.
                    let app = app.clone();
                    let mut prompt = String::new();
                    for block in &request.prompt {
                        match block {
                            ContentBlock::Text(text) => prompt.push_str(&text.text),
                            // Nothing beyond text is advertised, so a
                            // non-text block is a client bug worth
                            // naming.
                            _ => {
                                return Err(Error::invalid_request()
                                    .data("only text prompt blocks are supported"));
                            }
                        }
                    }
                    let session_id = request.session_id.clone();
                    // A prompt whose first line names one of the
                    // session's commands runs in place: the switch is
                    // answered as a completed turn without a model
                    // call, and the pickers re-render from the selects
                    // that ride along. Any other slash spelling,
                    // a path, an unknown command, is model input
                    // like any other text.
                    if let Some((name, args)) = slash_command(&prompt)
                        && let Some(answer) = app.command(&session_id.0, &name, &args)?
                    {
                        app.remember_title(&session_id.0, &prompt);
                        let _ = cx.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(answer.message)),
                            )),
                        ));
                        if !answer.selects.is_empty() {
                            let _ = cx.send_notification(SessionNotification::new(
                                session_id.clone(),
                                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                                    answer.selects,
                                )),
                            ));
                        }
                        app.note_activity(&session_id.0, &cx);
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                        return Ok(());
                    }
                    let Some((agent, memory)) = app.take_agent(&session_id.0) else {
                        return Err(Error::invalid_request()
                            .data("unknown session, or a turn is already running for it"));
                    };
                    app.remember_title(&session_id.0, &prompt);
                    let (meter, context_size, pricing) = {
                        let sessions = app.lock();
                        let slot = sessions.get(session_id.0.as_ref());
                        (
                            slot.map(|slot| slot.meter.clone()),
                            slot.and_then(|slot| slot.context_size),
                            slot.and_then(|slot| slot.pricing),
                        )
                    };
                    let Some(meter) = meter else {
                        return Err(Error::invalid_request()
                            .data("unknown session, or a turn is already running for it"));
                    };
                    let (cancel, cancelled) = watch::channel(false);
                    app.set_cancel(&session_id.0, Some(cancel));
                    let mut sink = AcpSink::new(cx.clone(), session_id.clone()).measuring(
                        meter.clone(),
                        context_size,
                        pricing,
                    );
                    let approval =
                        Arc::new(AcpApproval::new(cx.clone(), session_id.clone(), memory));
                    // An independent tokio task, not `cx.spawn`: the
                    // SDK polls its spawned tasks inside the connection
                    // future, where a turn parked on a permission answer
                    // would hold the very dispatch loop that must route
                    // that answer. Off the connection future, the park
                    // is a plain runtime await and traffic keeps
                    // flowing. The connection survives the turn's end:
                    // every send below fails harmlessly once it closes.
                    tokio::spawn(async move {
                        // The guard holds the agent while the turn runs:
                        // the arms below take it out and restore it
                        // themselves, and a panic unwinding past them,
                        // the turn's own, most likely, is what the drop
                        // covers, so the slot's agent always comes home
                        // and the slot keeps serving prompts.
                        let mut restore = SlotRestore {
                            app: app.clone(),
                            id: Arc::clone(&session_id.0),
                            agent: Some(agent),
                        };
                        // The block ends the pin's borrow of the agent
                        // before the match takes it out of the guard.
                        let turn_submission = {
                            let agent = restore.agent.as_mut().expect("the guard was just filled");
                            // The hook belongs to this turn's editor
                            // connection: a fresh round, a fresh
                            // approval bridge.
                            agent.set_approval(approval);
                            // Every stream boundary commits like the
                            // TUI's and headless' runs: a turn that
                            // dies mid-step leaves the last boundary
                            // on disk instead of losing the whole turn.
                            let commit = app
                                .base
                                .as_ref()
                                .map(|base| Arc::new(FileStore::new(base)))
                                .zip(StoreSessionId::new(&*session_id.0).ok());
                            if let Some((store, id)) = commit {
                                agent.set_commit_hook(Some(Box::new(move |session: &Session| {
                                    store.save(&id, session).map_err(Into::into)
                                })));
                            }
                            // The turn is a submission before it is a
                            // model call: the record is saved with the
                            // history before the first token is spent,
                            // so a resumed process can tell a turn that
                            // never started from one that died mid-step.
                            let at = rhema_submissions::now();
                            let submission = match rhema_submissions::turn::submit(
                                agent.session_mut(),
                                prompt.clone(),
                                at,
                            ) {
                                Ok(record) => record,
                                Err(err) => {
                                    let _ = responder.respond_with_error(
                                        Error::internal_error()
                                            .data(format!("cannot record the turn: {err}")),
                                    );
                                    return;
                                }
                            };
                            if let Err(err) = rhema_submissions::turn::start(
                                agent.session_mut(),
                                &submission.id,
                                &prompt,
                                at,
                            ) {
                                let _ = rhema_submissions::cancel(
                                    agent.session_mut(),
                                    &submission.id,
                                    at,
                                );
                                let _ = responder.respond_with_error(
                                    Error::internal_error()
                                        .data(format!("cannot record the turn: {err}")),
                                );
                                return;
                            }
                            // The record did not become durable: do not
                            // spend a token on work nothing records.
                            if let Err(err) = app.save(&session_id.0, agent.session()) {
                                let _ = rhema_submissions::settle(
                                    agent.session_mut(),
                                    &submission.id,
                                    rhema_submissions::Outcome::Cancelled,
                                    at,
                                );
                                let _ = responder.respond_with_error(
                                    Error::internal_error()
                                        .data(format!("cannot record the turn: {err}")),
                                );
                                return;
                            }
                            let turn = agent.turn(prompt, &mut sink);
                            tokio::pin!(turn);
                            let outcome = tokio::select! {
                                _ = cancelled_turn(cancelled) => None,
                                result = &mut turn => Some(result),
                            };
                            (outcome, submission.id)
                        };
                        let (outcome, turn_id) = turn_submission;
                        // The hook was this turn's; a later command on
                        // the slot must not checkpoint behind the
                        // post-turn save below.
                        if let Some(agent) = restore.agent.as_mut() {
                            agent.set_commit_hook(None);
                        }
                        match outcome {
                            Some(Ok(output)) => {
                                let mut agent = restore
                                    .agent
                                    .take()
                                    .expect("the guard still holds the agent");
                                // A turn still asking for tools at the cap
                                // is the protocol's max-turn-request stop,
                                // not a failure; its calls already ran and
                                // the history is valid for the next prompt.
                                let stop = if output.stop_reason == EngineStop::ToolUse {
                                    StopReason::MaxTurnRequests
                                } else {
                                    acp_stop(&output.stop_reason)
                                };
                                settle_turn(
                                    agent.session_mut(),
                                    &turn_id,
                                    rhema_submissions::Outcome::Done,
                                );
                                app.persist(&session_id.0, agent.session());
                                app.put_agent(&session_id.0, agent);
                                app.note_activity(&session_id.0, &cx);
                                let _ =
                                    responder.respond(prompt_response(stop, &app, &session_id.0));
                            }
                            Some(Err(err)) => {
                                let mut agent = restore
                                    .agent
                                    .take()
                                    .expect("the guard still holds the agent");
                                // The settle is the write; the repair
                                // below is for the next prompt only, so
                                // the file keeps what the turn left.
                                settle_turn(agent.session_mut(), &turn_id, failure_outcome(&err));
                                app.persist(&session_id.0, agent.session());
                                rhema_core::close_orphaned_calls(&mut agent.session_mut().history);
                                app.put_agent(&session_id.0, agent);
                                app.note_activity(&session_id.0, &cx);
                                let _ = responder.respond_with_error(err_to_error(&err));
                            }
                            // Dropped mid-turn by session/cancel: the
                            // history stays load-bearing for the next
                            // prompt, and the protocol asks for this
                            // stop reason even when the unwind errored.
                            None => {
                                let mut agent = restore
                                    .agent
                                    .take()
                                    .expect("the guard still holds the agent");
                                settle_turn(
                                    agent.session_mut(),
                                    &turn_id,
                                    rhema_submissions::Outcome::Cancelled,
                                );
                                app.persist(&session_id.0, agent.session());
                                rhema_core::close_orphaned_calls(&mut agent.session_mut().history);
                                app.put_agent(&session_id.0, agent);
                                app.note_activity(&session_id.0, &cx);
                                let _ = responder.respond(prompt_response(
                                    StopReason::Cancelled,
                                    &app,
                                    &session_id.0,
                                ));
                            }
                        }
                    });
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let app = app.clone();
                async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                    // The two selects the session advertises; anything
                    // else is a client bug worth naming.
                    let id = &request.session_id.0;
                    // A select option carries its value as a string id.
                    let Some(value) = request.value.as_value_id() else {
                        return Err(
                            Error::invalid_params().data("config options expect a string value")
                        );
                    };
                    // Rebuild the agent over the session's history on
                    // the new choice, across providers when a model
                    // choice names one; the updated selects are the
                    // response.
                    let switched = match request.config_id.0.as_ref() {
                        "model" => app.switch_model(id, &value.0),
                        "thinking" => app
                            .switch_thinking(id, &value.0)
                            .map(|(_, selects)| selects),
                        other => {
                            return Err(Error::invalid_params()
                                .data(format!("unknown config option: {other:?}")));
                        }
                    };
                    match switched {
                        Ok(config_options) => {
                            let _ = responder
                                .respond(SetSessionConfigOptionResponse::new(config_options));
                        }
                        Err(err) => return Err(err),
                    }
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let app = app.clone();
                async move |notification: CancelNotification, _cx| {
                    app.cancel(&notification.session_id.0);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    result.map_err(|err| anyhow::anyhow!("ACP connection failed: {err}"))
}

/// Resolves once the cancel flag flips; a dropped sender (the session
/// is gone) means never, the turn's own completion finishes the
/// select then.
async fn cancelled_turn(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A prompt that names one of the session's commands: the first line,
/// `/name` then free text. Only the names the session advertises
/// route; any other slash spelling, a path, a guess, is model input
/// like any other text.
fn slash_command(prompt: &str) -> Option<(String, String)> {
    let first = prompt.trim_start().lines().next()?;
    let (name, args) = first
        .strip_prefix('/')?
        .split_once(char::is_whitespace)
        .unwrap_or((first.strip_prefix('/')?, ""));
    let name = name.trim().to_ascii_lowercase();
    matches!(name.as_str(), "thinking" | "model").then(|| (name, args.trim().to_owned()))
}

/// Settles the prompt's turn submission, warning on stderr when the
/// transition is refused: the turn's answer is on disk either way.
fn settle_turn(
    session: &mut Session,
    id: &rhema_submissions::SubmissionId,
    outcome: rhema_submissions::Outcome,
) {
    let at = rhema_submissions::now();
    if let Err(err) = rhema_submissions::settle(session, id, outcome, at) {
        eprintln!("rhema-acp: warning: submission {id} was not settled: {err}");
    }
}

/// The outcome a failed turn settles with. The failure class is read
/// here, while the error is still in hand.
fn failure_outcome(err: &HostAgentError) -> rhema_submissions::Outcome {
    let retryable = matches!(
        err,
        HostAgentError::Agent(agent) if agent.is_retryable()
    );
    rhema_submissions::Outcome::Failed {
        error: err.to_string(),
        retryable,
    }
}
