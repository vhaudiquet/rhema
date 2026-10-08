//! The agent's configuration: model, prompt, workspace, turn budget.

use std::path::PathBuf;

/// Everything a [`CodingAgent`](crate::CodingAgent) needs besides the
/// provider.
#[derive(Debug, Clone)]
pub struct CodingAgentConfig {
    /// Model identifier, passed through to
    /// [`ChatParams`](rhema_core::ChatParams).
    pub model: String,
    /// System prompt stamped into every request. `None` uses the base
    /// prompt — [`base_system`](Self::base_system) when set, the
    /// built-in default otherwise; setting it *replaces* everything, it
    /// does not append.
    pub system: Option<String>,
    /// The base prompt from the user's `SYSTEM.md`, `{workspace}`
    /// placeholder included. The skills table appends after it; `None`
    /// uses the built-in [`default_system`](crate::default_system).
    pub base_system: Option<String>,
    /// Root all tools are scoped to; paths that escape it are rejected
    /// in-band to the model.
    pub workspace: PathBuf,
    /// Cap on steps per turn; a turn still requesting tools past it fails
    /// with `AgentError::MaxIterations`.
    pub max_turn_steps: u32,
}

impl CodingAgentConfig {
    /// Default for `max_turn_steps`. Long autonomous sessions — builds,
    /// rebases, multi-package sweeps — routinely run hundreds of steps on
    /// cheap models under a watching eye; the cap exists to end a runaway
    /// loop, not to ration the model. `--max-steps` is the tighter leash
    /// for unattended turns on expensive models.
    pub const DEFAULT_MAX_TURN_STEPS: u32 = 2048;

    /// Config naming `model`, rooted at `workspace`, defaults elsewhere.
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            system: None,
            base_system: None,
            workspace: workspace.into(),
            max_turn_steps: Self::DEFAULT_MAX_TURN_STEPS,
        }
    }
}
