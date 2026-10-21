//! The agent's configuration: model, prompt, workspace, turn budget.

use std::path::PathBuf;
use std::sync::Arc;

use rhema_agent_tools::{ShellTools, Skill};
use rhema_core::ToolResolver;
use rhema_core::request::ThinkingEffort;

/// Everything a [`CodingAgent`](crate::CodingAgent) needs besides the
/// provider.
#[derive(Clone)]
pub struct CodingAgentConfig {
    /// Model identifier, passed through to
    /// [`ChatParams`](rhema_core::ChatParams).
    pub model: String,
    /// System prompt stamped into every request. `None` uses the base
    /// prompt — [`base_system`](Self::base_system) when set, the
    /// built-in default otherwise; setting it *replaces* everything, it
    /// does not append.
    pub system: Option<String>,
    /// The base prompt from the user's `SYSTEM.md`, a Jinja template
    /// rendered by [`render_system`](crate::render_system) — a template
    /// error fails the agent's construction. `None` uses the built-in
    /// [`default_system`](crate::default_system).
    pub base_system: Option<String>,
    /// Root all tools are scoped to; paths that escape it are rejected
    /// in-band to the model.
    pub workspace: PathBuf,
    /// Skills already collected elsewhere, for the prompt's skills list.
    /// `None` discovers from the filesystem — the global config
    /// directory and the workspace's `.rhema/skills`. A frontend with no
    /// filesystem to discover from (the cloud sandbox) seeds the skill
    /// files itself and passes the list; the model reads bodies through
    /// the shell, so each path must resolve inside the workspace.
    pub skills: Option<Vec<Skill>>,
    /// Cap on steps per turn; a turn still requesting tools past it fails
    /// with `AgentError::MaxIterations`.
    pub max_turn_steps: u32,
    /// How much reasoning the model should do; providers without a knob
    /// ignore it, and [`ThinkingEffort::Default`] sends nothing.
    pub thinking: ThinkingEffort,
    /// The shell resolver the agent's tools run on. `None` builds one
    /// over `workspace` — the default. A frontend that shares its shell
    /// with an extension (the subagents extension reads the turn's
    /// approval hook off it) brings its own.
    pub tools: Option<Arc<ShellTools>>,
    /// An extension resolver merged over the shell's tools through
    /// [`Overlay`](rhema_agent_tools::Overlay): its specs join the
    /// prompt's tool list and its tools resolve by name, shadowing a
    /// built-in of the same name. `None` runs the built-ins alone.
    pub extra_resolver: Option<Arc<dyn ToolResolver>>,
    /// The transcript: a bounded request view over the unbounded
    /// session, plus the `zoom` tool to read back through it. On by
    /// default and below the budget a pass-through, so small sessions
    /// are byte-identical to a transcript-less run. `None` is the off
    /// switch: requests carry the whole history. See
    /// `specs/rhema-transcript.md`.
    pub transcript: Option<rhema_transcript::TranscriptSettings>,
}

impl std::fmt::Debug for CodingAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The resolvers are trait objects and stay opaque: their
        // presence is the debug-worthy fact, not their internals.
        f.debug_struct("CodingAgentConfig")
            .field("model", &self.model)
            .field("system", &self.system)
            .field("base_system", &self.base_system)
            .field("workspace", &self.workspace)
            .field("skills", &self.skills.is_some())
            .field("max_turn_steps", &self.max_turn_steps)
            .field("thinking", &self.thinking)
            .field("tools", &self.tools.is_some())
            .field("extra_resolver", &self.extra_resolver.is_some())
            .field("transcript", &self.transcript.is_some())
            .finish()
    }
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
            skills: None,
            max_turn_steps: Self::DEFAULT_MAX_TURN_STEPS,
            thinking: ThinkingEffort::Default,
            tools: None,
            extra_resolver: None,
            transcript: Some(rhema_transcript::TranscriptSettings::default()),
        }
    }
}
