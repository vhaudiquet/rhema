//! The coding-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop over `rhema_core::run_turn`.
//!
//! The agent owns one [`CodingTools`] resolver for its lifetime. Approval
//! arrives per turn; it is installed into the resolver before stepping and
//! cleared after (see [`crate::approval`] for why the hook travels as an
//! `Arc`). The session is plain data — the driver may mutate it between
//! turns, checkpoint it, or resume it from disk.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rhema_core::{
    Agent, AgentError, ChatParams, EventSink, Message, ModelOutput, Role, Session, ToolContext,
    run_turn,
};

use crate::approval::Approval;
use crate::config::CodingAgentConfig;
use crate::tools::CodingTools;

/// Why a turn or a session operation failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodingAgentError {
    /// The engine or the provider failed, including a turn that exceeded
    /// the step budget (`AgentError::MaxIterations`).
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// A path resolved outside the workspace root. Inside a turn this
    /// surfaces as an in-band `is_error` tool result instead (SPEC.md §4);
    /// the variant exists for callers validating paths outside the tool
    /// loop.
    #[error("workspace escape: {0}")]
    WorkspaceEscape(std::path::PathBuf),
}

/// The built-in base prompt, kept as data next to the code and read in
/// whole: `{workspace}` is the placeholder a session's workspace path
/// replaces.
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// The default base prompt for a workspace: [`DEFAULT_SYSTEM_MD`] with
/// the placeholder filled.
pub fn default_system(workspace: &Path) -> String {
    DEFAULT_SYSTEM_MD.replace("{workspace}", &workspace.display().to_string())
}

/// A coding agent: drives [`run_turn`] over the built-in tools, one turn at
/// a time, keeping the history in a caller-visible [`Session`].
pub struct CodingAgent {
    agent: Agent,
    config: CodingAgentConfig,
    session: Session,
    tools: Arc<CodingTools>,
    turns: u64,
}

impl CodingAgent {
    /// A fresh agent with an empty session. The workspace is canonicalized
    /// for tool scoping; the system prompt defaults to [`default_system`]
    /// unless `config.system` replaces it.
    pub fn new(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
    ) -> Result<Self, AgentError> {
        let tools = Arc::new(CodingTools::new(config.workspace.clone()));
        let system = config.system.clone().unwrap_or_else(|| {
            // An explicit base from the user's SYSTEM.md replaces the
            // built-in default; the skills table appends either way.
            let base = config
                .base_system
                .clone()
                .unwrap_or_else(|| default_system(tools.workspace()))
                .replace("{workspace}", &tools.workspace().display().to_string());
            // The global skills directory may not exist (no HOME) — as
            // normal as an empty one.
            let global = rhema_config::Config::skills_dir().ok();
            let skills = crate::skills::discover(global.as_deref(), tools.workspace());
            base + &crate::skills::prompt_section(&skills)
        });
        let agent = Agent::builder()
            .provider(provider)
            .resolver(tools.clone())
            .system(system)
            .params(ChatParams::new(config.model.clone()))
            .build()?;
        Ok(Self {
            agent,
            config,
            session: Session::default(),
            tools,
            turns: 0,
        })
    }

    /// A fresh agent resuming `session` (e.g. from
    /// [`CodingAgent::load_session_from`]); the next turn continues the
    /// prior history.
    pub fn from_session(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
        session: Session,
    ) -> Result<Self, AgentError> {
        let mut agent = Self::new(provider, config)?;
        agent.session = session;
        Ok(agent)
    }

    /// The conversation so far.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Mutable access between turns; the engine owns the history only
    /// inside a turn.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// One turn: push the user input, loop steps until the model stops
    /// requesting tools (capped at `max_turn_steps`), with `approval`
    /// gating the side-effecting tools. Appends to the session in place;
    /// the hook is cleared on the way out, success or failure.
    pub async fn turn(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.tools.install_approval(approval);
        let result = run_turn(
            &mut self.agent,
            &mut self.session,
            Message::text(Role::User, input),
            sink,
            &ctx,
            self.config.max_turn_steps,
        )
        .await;
        self.tools.clear_approval();
        Ok(result?)
    }

    /// Writes the session as pretty JSON to `path`.
    pub fn save_session_to(&self, path: &impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.session)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(path, json)
    }

    /// Reads a session written by [`CodingAgent::save_session_to`].
    pub fn load_session_from(path: &impl AsRef<Path>) -> io::Result<Session> {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}
