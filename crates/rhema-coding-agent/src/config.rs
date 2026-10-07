//! The agent's configuration: model, prompt, workspace, turn budget.

use std::path::PathBuf;

/// Everything a [`CodingAgent`](crate::CodingAgent) needs besides the
/// provider.
#[derive(Debug, Clone)]
pub struct CodingAgentConfig {
    /// Model identifier, passed through to
    /// [`ChatParams`](rhema_core::ChatParams).
    pub model: String,
    /// System prompt stamped into every request. `None` uses
    /// [`default_system`](crate::default_system); setting it *replaces* the
    /// default, it does not append.
    pub system: Option<String>,
    /// Root all tools are scoped to; paths that escape it are rejected
    /// in-band to the model.
    pub workspace: PathBuf,
    /// Cap on steps per turn; a turn still requesting tools past it fails
    /// with `AgentError::MaxIterations`.
    pub max_turn_steps: u32,
}

impl CodingAgentConfig {
    /// Default for `max_turn_steps`. Real agentic work regularly runs
    /// tens of steps; the cap exists to end a runaway loop, not to
    /// ration the model — and every step is a billed model call, so it
    /// still bounds an unattended turn.
    pub const DEFAULT_MAX_TURN_STEPS: u32 = 64;

    /// Config naming `model`, rooted at `workspace`, defaults elsewhere.
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            system: None,
            workspace: workspace.into(),
            max_turn_steps: Self::DEFAULT_MAX_TURN_STEPS,
        }
    }
}
