//! The ACP agent surface: one stdio connection, six handlers.
//!
//! Handlers follow the SDK's ordering contract. Everything that answers
//! immediately (initialize, authenticate, session/new, session/load,
//! session/cancel) runs inside the dispatch loop; `session/prompt` —
//! minutes long — spawns its turn task through [`ConnectionTo::spawn`]
//! and returns at once, so `session/cancel` and the client's permission
//! answers keep flowing while the turn runs.
//!
//! Session state lives in [`App`]: one slot per session holding the
//! [`CodingAgent`], a cancel sender, and the `always` permission memory.
//! The prompt task *takes* the agent out for the turn's duration, which
//! makes a second concurrent prompt for the same session a clean error
//! instead of a data race.
//!
//! Deliberately absent: MCP servers (`mcpServers` is accepted and
//! ignored — nothing is advertised), fs/terminal callbacks (the `bash`
//! tool does its own I/O), and session modes (nothing advertised, so
//! no client offers them). The one config option is the `Model` select
//! — the model catalog every logged-in provider offers — see
//! [`App::model_select`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthMethodId, AuthenticateRequest,
    AuthenticateResponse, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error, Stdio, on_receive_notification, on_receive_request,
};
use anyhow::Context;
use rhema_coding_agent::{CodingAgent, CodingAgentConfig, CodingAgentError, repair_dangling_calls};
use rhema_config::Config;
use rhema_core::request::ThinkingEffort;
use rhema_core::{AgentError, Content, ModelProvider, Role, Session, StopReason as EngineStop};
use tokio::sync::watch;

use crate::approval::{AcpApproval, AlwaysMemory};
use crate::backend;
use crate::sink::AcpSink;
use crate::store;

/// Everything the `rhema acp` flags contribute; the rest comes from the
/// stored config, exactly like the headless one-shot.
#[derive(Debug, Clone, Default)]
pub struct AcpOptions {
    /// Which config profile to use; `None` is the default profile.
    pub profile: Option<String>,
    /// Model backend override (a provider name like `google`,
    /// `anthropic`, `openai`, `deepseek`, `openrouter`, `llama-cpp`).
    pub provider: Option<String>,
    /// Model name override.
    pub model: Option<String>,
    /// llama.cpp server base URL override.
    pub llama_url: Option<String>,
    /// Cap on steps per turn; `None` is the coding agent's default.
    pub max_steps: Option<u32>,
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
    pub config: Option<rhema_config::Config>,
}

/// One live session: the agent between turns, the cancel sender of the
/// turn in flight, and the `always` answers the session has collected.
pub(crate) struct SessionSlot {
    /// The provider kind the session rides on — a [`backend::PROVIDERS`]
    /// name, or `"mock"` under the mock feature. A model switch may
    /// change it; the provider moves with it.
    kind: &'static str,
    /// The provider the agent's turns run on; a model switch that names
    /// another kind replaces it.
    provider: Arc<dyn ModelProvider>,
    /// Gone while a turn runs; the prompt task puts it back after.
    agent: Option<CodingAgent>,
    /// The model the agent runs on. A `session/set_config_option` that
    /// names a different model rebuilds the agent over the session's
    /// history; the slot's `model` is the current value the picker shows.
    model: String,
    /// The workspace root the agent's tools are scoped to; a model
    /// change rebuilds the agent over the same history and root.
    workspace: std::path::PathBuf,
    /// The catalog rows the model picker offers — every logged-in
    /// provider's, fetched once per `session/new`/`session/load`. A
    /// model switch re-renders the select from these rows without
    /// re-fetching.
    catalog_rows: Vec<backend::CatalogRow>,
    /// The watch sender the prompt task listens to; `None` when idle.
    cancel: Option<watch::Sender<bool>>,
    /// Shared with the session's approval hooks.
    memory: Arc<Mutex<AlwaysMemory>>,
}

/// The connection-wide state every handler closes over.
pub(crate) struct App {
    options: AcpOptions,
    /// The profile resolved once at startup: credentials, model, thinking.
    effective: rhema_config::Profile,
    /// The rendered base prompt from the user's SYSTEM.md, when present.
    base_system: Option<String>,
    /// Where sessions persist; `None` when no cache dir exists (turns
    /// still run; only `session/load` across restarts is lost).
    base: Option<std::path::PathBuf>,
    sessions: Mutex<HashMap<String, SessionSlot>>,
    next_id: AtomicU64,
}

impl App {
    /// The config in force: the test override when one is injected,
    /// the stored file otherwise.
    #[cfg(feature = "mock")]
    fn load_config(options: &AcpOptions) -> Result<Config, rhema_config::ConfigError> {
        if let Some(config) = &options.config {
            return Ok(config.clone());
        }
        Config::load()
    }

    #[cfg(not(feature = "mock"))]
    fn load_config(_options: &AcpOptions) -> Result<Config, rhema_config::ConfigError> {
        Config::load()
    }

    fn new(options: AcpOptions) -> Self {
        let config = match Self::load_config(&options) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("rhema-acp: warning: ignoring unreadable rhema config: {err}");
                Config::default()
            }
        };
        let effective = config.effective(options.profile.as_deref());
        let base_system = Config::system_prompt_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let base = options
            .session_dir
            .clone()
            .or_else(|| store::base_dir().ok());
        Self {
            options,
            effective,
            base_system,
            base,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// The backend the startup options and stored config resolve to;
    /// `None` is `auth_required` to the client. Under the `mock`
    /// feature, a scripted provider replaces resolution entirely.
    fn backend(&self) -> Option<backend::Resolved> {
        #[cfg(feature = "mock")]
        if let Some(replies) = &self.options.mock_replies {
            return Some(backend::Resolved {
                kind: "mock",
                provider: Arc::new(rhema_core::MockProvider::new(replies.iter().cloned())),
                model: self
                    .options
                    .model
                    .clone()
                    .unwrap_or_else(|| "mock".to_owned()),
            });
        }
        // A configuration problem (nothing configured, an unknown
        // stored spelling) is reported on stderr — the one channel left
        // — and reads to the client as `None`: `auth_required`, the
        // honest answer for a session that cannot start either way.
        match backend::resolve(
            &self.effective,
            self.options.provider.as_deref(),
            self.options.model.as_deref(),
            self.options.llama_url.as_deref(),
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("rhema-acp: {err}");
                None
            }
        }
    }

    /// The catalog rows the session's model picker offers: every
    /// provider the profile holds credentials for (the same set the
    /// TUI's `/model` picker offers). Under the `mock` feature there is
    /// no real provider: the scripted model alone is the row, so the
    /// picker still names it.
    async fn catalog(&self) -> Vec<backend::CatalogRow> {
        #[cfg(feature = "mock")]
        {
            // The mock row only stands when a scripted provider is
            // actually configured; a real backend that resolved beside
            // it is not ours to fake a catalog for.
            match self.options.mock_replies.as_ref() {
                Some(_) => {
                    let model = self
                        .options
                        .model
                        .clone()
                        .unwrap_or_else(|| "mock".to_owned());
                    vec![backend::CatalogRow {
                        provider: "mock",
                        card: rhema_provider::ModelCard {
                            id: model.clone(),
                            display_name: model,
                            description: String::new(),
                            input_token_limit: None,
                            output_token_limit: None,
                            generation_methods: vec!["generateContent".to_owned()],
                            pricing: None,
                        },
                    }]
                }
                None => Vec::new(),
            }
        }
        #[cfg(not(feature = "mock"))]
        backend::catalog(&self.effective, self.options.llama_url.as_deref()).await
    }

    fn thinking(&self) -> ThinkingEffort {
        self.options
            .thinking
            .or_else(|| self.effective.thinking_effort())
            .unwrap_or(ThinkingEffort::Default)
    }

    fn agent_config(&self, model: &str, workspace: &std::path::Path) -> CodingAgentConfig {
        CodingAgentConfig {
            max_turn_steps: self
                .options
                .max_steps
                .unwrap_or(CodingAgentConfig::DEFAULT_MAX_TURN_STEPS),
            thinking: self.thinking(),
            base_system: self.base_system.clone(),
            ..CodingAgentConfig::new(model, workspace)
        }
    }

    /// The `Model` select one session carries: the model it runs on is
    /// the current value — `<kind>/<model>` — and every catalog row is
    /// a choice, led by the current model when the catalog does not
    /// list it (a `--model` override, or its provider's slice failed to
    /// fetch). Each choice's description carries its provider and the
    /// card's facts — the same line the TUI's picker renders — so the
    /// client can show pricing and context window beside the name.
    fn model_select(
        &self,
        kind: &str,
        model: &str,
        rows: &[backend::CatalogRow],
    ) -> Vec<SessionConfigOption> {
        let current = format!("{kind}/{model}");
        let mut options = Vec::with_capacity(rows.len() + 1);
        let current_card = rows
            .iter()
            .find(|row| row.provider == kind && row.card.id == model)
            .map(|row| &row.card);
        options.push(
            SessionConfigSelectOption::new(current.clone(), model.to_owned()).description(
                match current_card {
                    Some(card) => card_description(kind, card),
                    None => kind.to_owned(),
                },
            ),
        );
        for row in rows {
            let value = format!("{}/{}", row.provider, row.card.id);
            if value == current {
                continue;
            }
            options.push(
                SessionConfigSelectOption::new(value, option_name(&row.card))
                    .description(card_description(row.provider, &row.card)),
            );
        }
        vec![
            SessionConfigOption::select("model", "Model", current, options)
                .category(Some(SessionConfigOptionCategory::Model)),
        ]
    }

    /// A fresh, unique, file-safe session id.
    fn fresh_id(&self) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("rhema-{millis:016x}-{n:03}")
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionSlot>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the agent out for a turn; `None` when the session is
    /// unknown or already running one.
    fn take_agent(&self, id: &str) -> Option<(CodingAgent, Arc<Mutex<AlwaysMemory>>)> {
        let mut sessions = self.lock();
        let slot = sessions.get_mut(id)?;
        let agent = slot.agent.take()?;
        Some((agent, slot.memory.clone()))
    }

    /// Installs a cancel sender for the turn about to run.
    fn set_cancel(&self, id: &str, cancel: Option<watch::Sender<bool>>) {
        if let Some(slot) = self.lock().get_mut(id) {
            slot.cancel = cancel;
        }
    }

    /// Fires the cancel flag of the session's running turn. No turn, no
    /// effect: cancellation is idempotent by protocol.
    fn cancel(&self, id: &str) {
        if let Some(slot) = self.lock().get(id) {
            if let Some(cancel) = &slot.cancel {
                let _ = cancel.send(true);
            }
        }
    }

    /// Puts the agent back after a turn and clears the cancel sender.
    fn put_agent(&self, id: &str, agent: CodingAgent) {
        if let Some(slot) = self.lock().get_mut(id) {
            slot.agent = Some(agent);
            slot.cancel = None;
        }
    }

    /// Swaps the session's model — and, when the choice names another
    /// provider kind, the provider with it: the agent is rebuilt over
    /// the session's existing history, so the conversation continues
    /// under the new model. Returns the updated `Model` select (the
    /// client re-renders its picker). An unknown session or a turn in
    /// flight is `invalid_request`; a bad choice value or a kind whose
    /// credentials do not resolve is `invalid_params` naming the fix.
    fn switch_model(&self, id: &str, value: &str) -> Result<Vec<SessionConfigOption>, Error> {
        // The swap runs under the lock; the select is built once it is
        // released, since `model_select` borrows `self` again.
        let (kind, model, rows) = {
            let mut sessions = self.lock();
            let slot = sessions.get_mut(id).ok_or_else(|| {
                Error::invalid_request()
                    .data("unknown session, or a turn is already running for it")
            })?;
            let (kind, model) = backend::split_switch(value, slot.kind)
                .map_err(|message| Error::invalid_params().data(message))?;
            let provider = match kind {
                Some(kind) => {
                    backend::provider_for(kind, &self.effective, self.options.llama_url.as_deref())
                        .map_err(|message| Error::invalid_params().data(message))?
                        .ok_or_else(|| {
                            Error::invalid_params().data(format!(
                                "no usable credentials for {kind} — log in once with the \
                         rhema TUI or set its API key"
                            ))
                        })?
                }
                None => slot.provider.clone(),
            };
            let kind = kind.unwrap_or(slot.kind);
            let session = slot
                .agent
                .as_ref()
                .ok_or_else(|| {
                    Error::invalid_request()
                        .data("unknown session, or a turn is already running for it")
                })?
                .session()
                .clone();
            let agent = CodingAgent::from_session(
                provider.clone(),
                self.agent_config(&model, &slot.workspace),
                session,
            )
            .map_err(|err| Error::internal_error().data(err.to_string()))?;
            slot.kind = kind;
            slot.provider = provider;
            slot.model = model.clone();
            slot.agent = Some(agent);
            let rows = slot.catalog_rows.clone();
            (kind, model, rows)
        };
        Ok(self.model_select(kind, &model, &rows))
    }

    /// Persists a session after a completed turn. Best effort: a failed
    /// write warns on stderr and the turn still succeeds; stdout stays
    /// reserved for protocol traffic.
    fn persist(&self, id: &str, session: &Session) {
        let Some(base) = &self.base else {
            return;
        };
        if let Err(err) = store::save_session(base, id, session) {
            eprintln!("rhema-acp: warning: session {id} was not saved: {err}");
        }
    }
}

/// Serves one ACP connection over stdin/stdout until the client hangs
/// up. The connection runs on a dedicated thread with its own
/// multi-thread runtime — the approval bridge parks workers with
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

/// Serves one ACP connection over `transport` — [`serve`] minus stdio,
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
    let app = Arc::new(App::new(options));
    let result = Agent
        .builder()
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
                async move |request: NewSessionRequest, responder, _cx| {
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
                    let agent = CodingAgent::new(
                        backend.provider,
                        app.agent_config(&backend.model, &request.cwd),
                    )
                    .map_err(|err| Error::internal_error().data(err.to_string()))?;
                    let id = app.fresh_id();
                    let catalog = app.catalog().await;
                    let config_options = app.model_select(backend.kind, &backend.model, &catalog);
                    let slot = SessionSlot {
                        kind: backend.kind,
                        provider,
                        agent: Some(agent),
                        model: backend.model.clone(),
                        workspace: request.cwd.clone(),
                        catalog_rows: catalog,
                        cancel: None,
                        memory: Arc::new(Mutex::new(AlwaysMemory::default())),
                    };
                    app.lock().insert(id.clone(), slot);
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
                    let session =
                        store::load_session(base, &request.session_id.0).map_err(|err| {
                            Error::invalid_params()
                                .data(format!("session {}: {err}", request.session_id.0))
                        })?;
                    let provider = backend.provider.clone();
                    let agent = CodingAgent::from_session(
                        backend.provider,
                        app.agent_config(&backend.model, &request.cwd),
                        session,
                    )
                    .map_err(|err| Error::internal_error().data(err.to_string()))?;
                    replay(&cx, &request.session_id, agent.session());
                    let id = request.session_id.0.to_string();
                    let catalog = app.catalog().await;
                    let config_options = app.model_select(backend.kind, &backend.model, &catalog);
                    let slot = SessionSlot {
                        kind: backend.kind,
                        provider,
                        agent: Some(agent),
                        model: backend.model.clone(),
                        workspace: request.cwd.clone(),
                        catalog_rows: catalog,
                        cancel: None,
                        memory: Arc::new(Mutex::new(AlwaysMemory::default())),
                    };
                    app.lock().insert(id.clone(), slot);
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
                    let Some((mut agent, memory)) = app.take_agent(&session_id.0) else {
                        return Err(Error::invalid_request()
                            .data("unknown session, or a turn is already running for it"));
                    };
                    let (cancel, cancelled) = watch::channel(false);
                    app.set_cancel(&session_id.0, Some(cancel));
                    let mut sink = AcpSink::new(cx.clone(), session_id.clone());
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
                        // The block ends the pin's borrow of `agent`
                        // before the match moves it back into its slot.
                        let outcome = {
                            let turn = agent.turn(prompt, &mut sink, approval);
                            tokio::pin!(turn);
                            tokio::select! {
                                _ = cancelled_turn(cancelled) => None,
                                result = &mut turn => Some(result),
                            }
                        };
                        match outcome {
                            Some(Ok(output)) => {
                                let stop = acp_stop(&output.stop_reason);
                                app.persist(&session_id.0, agent.session());
                                app.put_agent(&session_id.0, agent);
                                let _ = responder.respond(PromptResponse::new(stop));
                            }
                            Some(Err(CodingAgentError::Agent(AgentError::MaxIterations(_)))) => {
                                // The budget ran out with calls possibly
                                // left hanging; close them before the
                                // session serves another turn.
                                repair_dangling_calls(agent.session_mut());
                                app.put_agent(&session_id.0, agent);
                                let _ = responder
                                    .respond(PromptResponse::new(StopReason::MaxTurnRequests));
                            }
                            Some(Err(err)) => {
                                repair_dangling_calls(agent.session_mut());
                                app.put_agent(&session_id.0, agent);
                                let _ = responder.respond_with_error(err_to_error(&err));
                            }
                            // Dropped mid-turn by session/cancel: the
                            // history stays load-bearing for the next
                            // prompt, and the protocol asks for this
                            // stop reason even when the unwind errored.
                            None => {
                                repair_dangling_calls(agent.session_mut());
                                app.put_agent(&session_id.0, agent);
                                let _ =
                                    responder.respond(PromptResponse::new(StopReason::Cancelled));
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
                    // Only the `model` select is advertised, so a config id
                    // that is not `model` is a client bug; name it.
                    if request.config_id.0.as_ref() != "model" {
                        return Err(Error::invalid_params().data(format!(
                            "unknown config option: \"{}\"",
                            request.config_id.0
                        )));
                    }
                    // A select option carries its value as a string id.
                    let Some(model) = request.value.as_value_id() else {
                        return Err(
                            Error::invalid_params().data("model option expects a string value")
                        );
                    };
                    // Rebuild the agent over the session's history on the
                    // new model — across providers when the choice names
                    // one; the updated select is the response.
                    match app.switch_model(&request.session_id.0, &model.0) {
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
/// is gone) means never — the turn's own completion finishes the
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

/// A model select choice's label: the display name when the listing
/// knows one, the id otherwise.
fn option_name(card: &rhema_provider::ModelCard) -> String {
    if card.display_name.is_empty() {
        card.id.clone()
    } else {
        card.display_name.clone()
    }
}

/// A model select choice's description: the provider kind serving the
/// model, then the card's facts — pricing and context window — when
/// the card knows them. The same line the TUI's picker renders; a
/// client shows it beside the choice's name.
fn card_description(provider: &str, card: &rhema_provider::ModelCard) -> String {
    let mut parts = vec![provider.to_owned()];
    if let Some(pricing) = card.pricing {
        parts.push(format!(
            "${:.2}/M in · ${:.2}/M out",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    if let Some(limit) = card.input_token_limit {
        parts.push(format!("{} ctx", format_tokens(limit)));
    }
    parts.join(" · ")
}

/// Humanizes a token count the way the TUI's picker does: 1_048_576 →
/// "1.0M", 131_072 → "131k", small counts stay raw.
fn format_tokens(limit: u64) -> String {
    if limit >= 1_000_000 {
        format!("{:.1}M", limit as f64 / 1_000_000.0)
    } else if limit >= 10_000 {
        format!("{}k", limit / 1_000)
    } else {
        format!("{limit}")
    }
}

/// A driver failure on the wire: an internal error carrying the
/// driver's own text.
fn err_to_error(err: &CodingAgentError) -> Error {
    Error::internal_error().data(err.to_string())
}

/// The engine's stop reasons on the ACP wire. A completed turn never
/// stops on `ToolUse` (the turn loop resolves those), and the remaining
/// engine reasons all read as a normal end of turn.
fn acp_stop(reason: &EngineStop) -> StopReason {
    match reason {
        EngineStop::MaxTokens => StopReason::MaxTokens,
        EngineStop::EndTurn | EngineStop::StopSequence | EngineStop::Other(_) => {
            StopReason::EndTurn
        }
        EngineStop::ToolUse => StopReason::EndTurn,
    }
}

/// Replays a restored history as session updates, live order, before
/// the `session/load` response: user and assistant text as message
/// chunks, calls with their results as completed or failed tool calls.
fn replay(cx: &ConnectionTo<Client>, session_id: &SessionId, session: &Session) {
    for message in &session.history {
        for content in &message.content {
            let update = match (message.role, content) {
                (Role::User, Content::Text(text)) => Some(SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone()))),
                )),
                (Role::Assistant, Content::Text(text)) => Some(SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone()))),
                )),
                (Role::Assistant, Content::ToolCall(call)) => {
                    let title = call
                        .args
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| call.name.clone());
                    Some(SessionUpdate::ToolCall(
                        ToolCall::new(ToolCallId::new(call.id.clone()), title)
                            .name(call.name.clone())
                            .kind(ToolKind::Execute),
                    ))
                }
                (Role::Tool, Content::ToolResult(result)) => {
                    Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(result.id.clone()),
                        ToolCallUpdateFields::new()
                            .status(if result.is_error {
                                ToolCallStatus::Failed
                            } else {
                                ToolCallStatus::Completed
                            })
                            .content(vec![ToolCallContent::from(ContentBlock::Text(
                                TextContent::new(result.content.clone()),
                            ))]),
                    )))
                }
                _ => None,
            };
            if let Some(update) = update {
                let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
            }
        }
    }
}
