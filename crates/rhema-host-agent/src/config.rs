//! The agent's configuration: model, prompt, workspace, turn budget.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use rhema_bash::{Approval, ShellTools};
use rhema_core::ToolResolver;
use rhema_core::request::ThinkingEffort;
use rhema_prompt::Skill;

/// Finished-work notifications a turn loop injects into the history.
///
/// The contract is deliberately text-shaped: the driver knows nothing
/// about what the lines announce, a registry of background jobs, an
/// inbox, anything, and the source knows nothing about turns or
/// history. `drain` is a query, never a mutation of record: the same
/// lines may come back again (a crash between the append and the
/// checkpoint only re-delivers), and the driver deduplicates within a
/// turn.
pub trait Steering: Send + Sync {
    /// The lines to append to the history now, one notification per
    /// line, in announcement order. Empty means nothing to announce.
    fn drain(&self) -> Vec<String>;
}

/// Several steering sources as one, in announcement order: a frontend
/// with two feeds, the subagent registry and the background-shell
/// poller, hands the driver one source, so the two announcements
/// coalesce into one User message with a stable order instead of
/// racing for the config's one `steering` slot.
pub struct SteeringChain {
    sources: Vec<Arc<dyn Steering>>,
}

impl SteeringChain {
    /// Chains `sources`, first drained first. An empty chain drains
    /// empty; a single source behaves as itself.
    pub fn new(sources: Vec<Arc<dyn Steering>>) -> Self {
        Self { sources }
    }
}

impl Steering for SteeringChain {
    fn drain(&self) -> Vec<String> {
        self.sources
            .iter()
            .flat_map(|source| source.drain())
            .collect()
    }
}

/// How a frontend resolves any provider kind to a turn provider: the
/// credential policy behind the subagent tool's cross-provider
/// `model` argument. A `None` answer means that kind has no usable
/// credentials here, the same shape the model picker's catalog
/// reports. The registry's `build` does the wiring; what this closure
/// adds is the resolved credentials themselves.
pub type SubagentProviders =
    Arc<dyn Fn(&str) -> Option<Arc<dyn rhema_core::ModelProvider>> + Send + Sync>;

/// Everything a [`HostAgent`](crate::HostAgent) needs: the backend
/// it runs on, the model, and the settings that shape its turns.
#[derive(Clone)]
pub struct HostAgentConfig {
    /// The model backend the agent runs on. [`HostAgent::new`] wraps
    /// it (the image mask, the transcript view) before the engine
    /// calls it; the field keeps the provider the config was built
    /// with, so a clone of the config builds another agent safely.
    ///
    /// [`HostAgent::new`]: crate::HostAgent::new
    pub provider: Arc<dyn rhema_core::ModelProvider>,
    /// The approval hook every gated tool call goes through. The
    /// constructor takes it beside the provider because there is no
    /// neutral default: `DenyAll` leaves the agent unable to act and
    /// `AllowAll` removes the gate silently. A driver that changes
    /// policy between turns swaps it with
    /// [`HostAgent::set_approval`](crate::HostAgent::set_approval).
    pub approval: Arc<dyn Approval>,
    /// Model identifier, passed through to
    /// [`ChatParams`](rhema_core::ChatParams).
    pub model: String,
    /// Whether the model accepts image input: the tri-state from its
    /// catalog card. [`HostAgent::new`] installs an image mask when
    /// this is `Some(false)`, so a request carries a text note where an
    /// image block would be and no base64 reaches a model that cannot
    /// read it. The session keeps its images, so a switch back to a
    /// vision model unmasks them by rebuilding the agent. `None` is
    /// unknown, not "no", and sends the images untouched.
    ///
    /// [`HostAgent::new`]: crate::HostAgent::new
    pub supports_vision: Option<bool>,
    /// System prompt stamped into every request. `None` uses the base
    /// prompt, [`base_system`](Self::base_system) when set, the
    /// built-in default otherwise; setting it *replaces* everything, it
    /// does not append.
    pub system: Option<String>,
    /// The base prompt from the user's `SYSTEM.md`, a Jinja template
    /// rendered by [`render_system`](crate::render_system), a template
    /// error fails the agent's construction. `None` uses the built-in
    /// [`default_system`](crate::default_system).
    pub base_system: Option<String>,
    /// Root all tools are scoped to; paths that escape it are rejected
    /// in-band to the model.
    pub workspace: PathBuf,
    /// Skills already collected elsewhere, for the prompt's skills list.
    /// `None` discovers from the filesystem, the global config
    /// directory and the workspace's `.rhema/skills`. A frontend with no
    /// filesystem to discover from (the cloud sandbox) seeds the skill
    /// files itself and passes the list; the model reads bodies through
    /// the shell, so each path must resolve inside the workspace.
    pub skills: Option<Vec<Skill>>,
    /// Skill folder names to drop from whichever list the prompt carries
    ///, the persisted off switch a frontend reads out of its config.
    /// The filter runs after collection and after discovery alike, so
    /// both supply paths honor it; an empty set (the default) changes
    /// nothing.
    pub disabled_skills: std::collections::BTreeSet<String>,
    /// Cap on steps per turn. A turn still requesting tools past it
    /// returns its last output with [`StopReason::ToolUse`], so the
    /// driver can show the reply and continue; `NonZeroU32` keeps a turn
    /// from starting with no step budget at all.
    ///
    /// [`StopReason::ToolUse`]: rhema_core::StopReason::ToolUse
    pub max_turn_steps: NonZeroU32,
    /// The conversation's stable id, stamped into every request's
    /// [`ChatParams::session_id`] for the dialects that key routing or
    /// caching on it (OpenRouter's sticky-routing key). A driver that
    /// persists its conversations names its own (rhema-code: the
    /// checkpoint's id, stable across a restart); `None` sends nothing,
    ///
    /// [`ChatParams::session_id`]: rhema_core::request::ChatParams::session_id
    /// and the server falls back to its own conversation identity.
    pub session_id: Option<String>,
    /// How much reasoning the model should do; providers without a knob
    /// ignore it, and [`ThinkingEffort::Default`] sends nothing.
    pub thinking: ThinkingEffort,
    /// The shell resolver the agent's tools run on. `None` builds one
    /// over `workspace`, the default. A frontend that shares its shell
    /// with an extension (the subagents extension reads the turn's
    /// approval hook off it) brings its own.
    pub tools: Option<Arc<ShellTools>>,
    /// An extension resolver merged over the shell's tools through
    /// [`Overlay`](rhema_core::Overlay): its specs join the
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
    /// The compaction's model fill: a cheap worker the transcript's
    /// stepped compaction runs at flushes, rewriting each new block
    /// section's extractive stand-in into a dense markdown block. The
    /// worker never blocks or fails a request, a failed call leaves
    /// the stand-in in place. `None` keeps the compaction extractive:
    /// still stepped, still cache-stable, only less dense. See
    /// `specs/rhema-transcript.md`.
    pub summary_worker: Option<Arc<dyn rhema_transcript::Summarizer>>,
    /// Extra environment the shell tool's commands run with, layered
    /// over the inherited one, how a frontend provisions its agent's
    /// client tools (a daemon URL, a scoped token, a PATH addition)
    /// without touching the process environment.
    pub shell_env: std::collections::BTreeMap<String, String>,
    /// A steering source the turn loop drains: one User-role message per
    /// drain lands in the history at a step boundary, so work that
    /// finished outside the turn (a background subagent) is announced to
    /// the model. `None`, the default, runs the turn exactly as
    /// before: the loop is the provided `run_turn` with no injection.
    pub steering: Option<Arc<dyn Steering>>,
    /// The prompt's `mcp` section, verbatim: the output of `mcp
    /// prompt` in the session, produced by the sandbox's client. `None`
    /// renders no section. The list is the client's, not the host's:
    /// the config lives where the servers do.
    pub mcp: Option<String>,
    /// Subagent definition folder names the subagent wire drops from
    /// discovery, the persisted off switch a frontend reads out of
    /// its stored settings, the [`disabled_skills`](Self::disabled_skills)
    /// mechanism. The filter runs before the catalog builds, so a
    /// disabled name is in neither the prompt's tool list nor the
    /// tools array; the built-in `general-purpose` answers to its name
    /// like any other, and a set holding every discovered name leaves
    /// no catalog, which drops the tool whole.
    pub disabled_subagents: std::collections::BTreeSet<String>,
    /// Resolves a provider kind to a turn provider, for the subagent
    /// tool's `model` argument (`<provider>/<model>`) and the same
    /// spelling in a definition's `model` frontmatter: a subagent can
    /// run on another logged-in provider while the session stays put.
    /// The wire hands it to the tool; `None` means the session's own
    /// provider is the only one, and cross-provider requests fail with
    /// that named. Credential resolution stays with the frontend,
    /// this carries the answers, not the policy.
    pub subagent_providers: Option<SubagentProviders>,
    /// The environment section rendered into the system prompt: machine
    /// facts probed where commands run (see the [`environment`
    /// module](crate::environment)). `None` renders the section empty.
    /// A verbatim `system` prompt ignores it, like every template
    /// section.
    pub environment: Option<String>,
    /// The user-preferences section rendered into the system prompt,
    /// the contents of the user's global `USER.md`, verbatim, a
    /// frontend that has one reads it and passes it here. `None`
    /// renders nothing.
    pub user_preferences: Option<String>,
    /// The prompt section documenting the wired
    /// [`vision`](rhema_bash::vision) macro, the `see` bash
    /// function's documentation, produced by the frontend's
    /// provisioning and passed verbatim. `None` renders no section: a
    /// frontend that wired nothing advertises nothing, the same rule
    /// the `mcp` section follows.
    pub vision: Option<String>,
    /// The prompt section documenting the wired
    /// [`jobs`](rhema_bash::jobs) macro, the `job` bash
    /// function's documentation, produced by the frontend's provisioning
    /// and passed verbatim. `None` renders no section: a frontend that
    /// wired nothing (a sandbox session, a machine without the backend)
    /// advertises nothing, the same rule [`vision`](Self::vision)
    /// follows.
    pub jobs: Option<String>,
    /// The prompt section documenting the wired
    /// [`websearch`](rhema_bash::websearch) macro, the `search`
    /// bash function's documentation, produced by the frontend's
    /// provisioning and passed verbatim. `None` renders no section: a
    /// frontend that wired nothing (a sandbox session, a remote
    /// executor, no providers configured) advertises nothing, the
    /// same rule [`vision`](Self::vision) follows.
    pub websearch: Option<String>,
    /// The prompt section documenting the wired
    /// [`decision`](rhema_bash::decision) macro, the `decide` bash
    /// function's documentation, produced by the frontend's
    /// provisioning and passed verbatim. `None` renders no section: a
    /// frontend that wired nothing (a sandbox session, no decision
    /// providers configured) advertises nothing, the same rule
    /// [`vision`](Self::vision) follows.
    pub decision: Option<String>,
    /// Prompt sections this crate does not name: one template variable
    /// per entry, values as JSON. The renderer places each exactly as it
    /// places the named sections above, and an entry that reuses one of
    /// their names replaces it. A frontend wiring a macro of its own (a
    /// `hear` function beside `see`, say) puts its section under the
    /// variable its template spells, so the crate needs no new field for
    /// it.
    pub sections: std::collections::BTreeMap<String, serde_json::Value>,
    /// The directory conventions skill discovery reads when
    /// [`skills`](Self::skills) is `None`: the user-wide skills directory
    /// and the cross-agent `.agents/skills` convention. `None`, the
    /// default, discovers the reference layout
    /// ([`Home::discover`](rhema_config::Home::discover)); a frontend
    /// that owns its own directory hands a relocated `Home` instead.
    pub home: Option<rhema_config::Home>,
}

impl std::fmt::Debug for HostAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The resolvers are trait objects and stay opaque: their
        // presence is the debug-worthy fact, not their internals.
        f.debug_struct("HostAgentConfig")
            .field("provider", &"<dyn ModelProvider>")
            .field("approval", &"<dyn Approval>")
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
            .field("summary_worker", &self.summary_worker.is_some())
            .field("steering", &self.steering.is_some())
            .field("shell_env", &self.shell_env.len())
            .field("environment", &self.environment.is_some())
            .field("user_preferences", &self.user_preferences.is_some())
            .field("home", &self.home.is_some())
            .finish()
    }
}

impl HostAgentConfig {
    /// Default for `max_turn_steps`. Long autonomous sessions, builds,
    /// rebases, multi-package sweeps, routinely run hundreds of steps on
    /// cheap models under a watching eye; the cap exists to end a runaway
    /// loop, not to ration the model. `--max-steps` is the tighter leash
    /// for unattended turns on expensive models.
    pub const DEFAULT_MAX_TURN_STEPS: NonZeroU32 = NonZeroU32::new(2048).unwrap();

    /// Config running `provider` on `model`, rooted at `workspace`,
    /// gating side effects through `approval`; defaults elsewhere.
    ///
    /// Both handles arrive as `impl Trait` and are wrapped here, so a
    /// caller holding an `Arc` passes it unchanged (the blanket impls
    /// in `rhema-core` and `rhema-bash` make `Arc<T>` a `T`).
    /// For an `Arc` argument the config keeps one more handle, not a
    /// copy of the provider.
    pub fn new(
        provider: impl rhema_core::ModelProvider + 'static,
        model: impl Into<String>,
        workspace: impl Into<PathBuf>,
        approval: impl Approval + 'static,
    ) -> Self {
        Self {
            provider: Arc::new(provider),
            approval: Arc::new(approval),
            model: model.into(),
            supports_vision: None,
            system: None,
            base_system: None,
            workspace: workspace.into(),
            skills: None,
            disabled_skills: std::collections::BTreeSet::new(),
            max_turn_steps: Self::DEFAULT_MAX_TURN_STEPS,
            session_id: None,
            thinking: ThinkingEffort::Default,
            tools: None,
            extra_resolver: None,
            transcript: Some(rhema_transcript::TranscriptSettings::default()),
            summary_worker: None,
            steering: None,
            shell_env: std::collections::BTreeMap::new(),
            mcp: None,
            disabled_subagents: std::collections::BTreeSet::new(),
            subagent_providers: None,
            environment: None,
            user_preferences: None,
            vision: None,
            jobs: None,
            websearch: None,
            decision: None,
            sections: std::collections::BTreeMap::new(),
            home: None,
        }
    }
}
