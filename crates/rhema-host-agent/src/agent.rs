//! The host-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop, the provided `rhema_core::run_turn`, or, with a
//! steering source configured, the driver's own loop with the two drain
//! points that append finished-work notifications at step boundaries.
//!
//! The agent owns one [`ShellTools`] resolver for its lifetime. The
//! config carries the approval hook; every turn installs it into the
//! resolver before stepping and clears it after (see
//! [`rhema_bash::approval`] for why the hook travels as an
//! `Arc`). A driver whose hook belongs to one turn, the TUI's
//! per-round bridge, swaps it with [`HostAgent::set_approval`].
//! The session is plain data, the driver may mutate it between turns,
//! checkpoint it, or resume it from disk. A turn entering with
//! committed-but-unanswered calls (a crash resume) revives first:
//! replay-safe tools re-run, the rest are reported interrupted, see
//! [`revive_dangling_calls`].

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use rhema_bash::{Approval, ShellTools};
use rhema_core::{
    Agent, AgentError, ChatParams, CommitHook, EventSink, Message, ModelOutput, Overlay, Role,
    Session, StopReason, ToolContext, ToolResolver, ToolSpec, revive_dangling_calls, run_turn,
};
use rhema_prompt::Skill;

use rhema_session_store::StoreError;
use rhema_session_store_file::PathStore;

use crate::config::{HostAgentConfig, Steering};

/// The driver-state key carrying the conversation's rendered
/// environment section: stamped at the conversation's start, reused by
/// every resume, so the system prompt's bytes, and with them the
/// provider's prompt cache for the whole prefix, survive rebuilds.
const ENVIRONMENT_STATE_KEY: &str = "environment_report";

/// Clears a session's stamped environment report, the transition hook
/// for rebuilds that change the facts the report carries: a machine
/// switch (a sandbox picked mid-conversation re-reports where `bash`
/// runs) or a model switch (the report names the model, and the
/// provider's cache is per-model regardless). Transitions that change
/// neither, a thinking level, a resume, keep the stamp: the report is
/// conversation state, and re-rendering it would rewrite the system
/// prompt and break the prefix cache for nothing.
pub fn clear_environment_stamp(session: &mut Session) {
    if let Some(state) = session.driver_state.as_object_mut() {
        state.remove(ENVIRONMENT_STATE_KEY);
    }
}

/// Why a turn or a session operation failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostAgentError {
    /// The engine or the provider failed. A turn that reaches its step
    /// budget is not an error: it returns its last output with
    /// `StopReason::ToolUse`.
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// A path resolved outside the workspace root. Inside a turn this
    /// surfaces as an in-band `is_error` tool result instead (SPEC.md §4);
    /// the variant exists for callers validating paths outside the tool
    /// loop.
    #[error("workspace escape: {0}")]
    WorkspaceEscape(std::path::PathBuf),
    /// The system-prompt template failed to render, bad syntax, unknown
    /// filter, with the template position when known. The agent refuses
    /// to start on a prompt it cannot fully render.
    #[error("system prompt template: {0}")]
    Template(String),
}

impl From<minijinja::Error> for HostAgentError {
    fn from(err: minijinja::Error) -> Self {
        let line = err
            .line()
            .map(|line| format!(", line {line}"))
            .unwrap_or_default();
        Self::Template(format!("{err}{line}"))
    }
}

/// The system prompt an agent stamps into every request, assembled
/// outside an agent: `base`, the user's SYSTEM.md, when given, the
/// built-in default otherwise, rendered over `workspace` as given, the
/// discovered skills, and the tool specs, the built-ins plus
/// `extra`, the extension resolver's specs, so the prompt and the
/// request's tools array cannot drift. The one assembly point:
/// [`HostAgent::new`] builds every agent's prompt here, and a driver
/// that previews the prompt calls the same function, so what is
/// previewed is what turns send. No optional sections: the environment
/// report and the user preferences render empty.
pub fn stamped_system(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
) -> Result<String, HostAgentError> {
    // Either global directory may be missing (no HOME), as normal as
    // an empty one.
    let home = rhema_config::Home::discover().ok();
    let agents = home.as_ref().map(|h| h.agents_skills());
    let global = home.as_ref().map(|h| h.skills());
    let skills = rhema_skills::discover(agents.as_deref(), global.as_deref(), workspace);
    stamped_system_with_sections(base, workspace, extra, &skills, &[])
}

/// [`stamped_system`] over skills collected elsewhere, the input path
/// for frontends with no filesystem to discover from: the cloud
/// deployment keeps skills in its durable storage, seeds them into the
/// sandbox workspace, and feeds the list here. The model still reads a
/// skill's body through the shell, so each `Skill::path` must resolve
/// inside the workspace the tools run on.
pub fn stamped_system_with_skills(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
    skills: &[Skill],
) -> Result<String, HostAgentError> {
    stamped_system_with_sections(base, workspace, extra, skills, &[])
}

/// The full assembly: the template over `workspace`, the skills, the
/// tool specs, and the named sections the template references.
///
/// Sections travel as pairs (`("environment", json!("- OS: linux"))`),
/// not as parameters, so the set is the caller's: the standard ones a
/// frontend renders (`environment`, `user`, `vision`, `jobs`,
/// `websearch`, `decision`, `mcp`) and any a frontend of its own adds (`hear`, say)
/// take the same path. The template decides whether and where each
/// appears, and a section that is absent renders as an undefined
/// variable, which is to say nothing.
pub fn stamped_system_with_sections(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
    skills: &[Skill],
    sections: &[(&str, serde_json::Value)],
) -> Result<String, HostAgentError> {
    let mut tools = ShellTools::builtin_specs();
    tools.extend_from_slice(extra);
    let prompt = rhema_prompt::Prompt::new(workspace, skills, &tools)
        .with_sections(sections.iter().map(|(name, value)| (*name, value.clone())));
    rhema_prompt::render_system(base.unwrap_or(crate::prompt::DEFAULT_SYSTEM_MD), &prompt)
        .map_err(HostAgentError::from)
}

/// The prompt sections a config carries, in one list: the six the
/// config names, then the MCP rows the wire step produced, then
/// [`HostAgentConfig::sections`], the caller's own.
///
/// One list on purpose. When each section was an argument to the render,
/// a field could ship without its argument, and `websearch` did: the
/// config set it and the prompt never carried it. Here a section the
/// config holds is a section the prompt has.
fn config_sections<'a>(
    config: &'a HostAgentConfig,
    mcp: &'a [rhema_config::mcp::McpServerLine],
) -> Vec<(&'a str, serde_json::Value)> {
    let mut sections: Vec<(&str, serde_json::Value)> = Vec::new();
    for (name, text) in [
        ("environment", config.environment.as_deref()),
        ("user", config.user_preferences.as_deref()),
        ("vision", config.vision.as_deref()),
        ("jobs", config.jobs.as_deref()),
        ("websearch", config.websearch.as_deref()),
        ("decision", config.decision.as_deref()),
    ] {
        if let Some(text) = text {
            sections.push((name, serde_json::json!(text)));
        }
    }
    if !mcp.is_empty() {
        sections.push(("mcp", serde_json::json!(mcp)));
    }
    sections.extend(
        config
            .sections
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone())),
    );
    sections
}

/// A host agent: drives [`run_turn`] over the built-in tools, one turn at
/// a time, keeping the history in a caller-visible [`Session`].
pub struct HostAgent {
    agent: Agent,
    config: HostAgentConfig,
    session: Session,
    tools: Arc<ShellTools>,
    /// The composed resolver, shell tools plus every extension (zoom,
    /// subagents). Revive classifies dangling calls against it, so an
    /// extension tool's replay-safety answer is honored; `tools` alone
    /// would answer `Unsafe` for every name.
    resolver: Arc<dyn ToolResolver>,
    /// The transcript's shared mirror, when the config enables it: the
    /// benchmark stats read from here.
    transcript: Option<Arc<Mutex<rhema_transcript::Transcript>>>,
    turns: u64,
}

impl HostAgent {
    /// A fresh agent with an empty session. The workspace is canonicalized
    /// for tool scoping; the system prompt defaults to the built-in
    /// template rendered by [`crate::render_system`] unless
    /// `config.system` replaces it. A base prompt that fails to render
    /// fails the construction.
    pub fn new(config: HostAgentConfig) -> Result<Self, HostAgentError> {
        let provider: Arc<dyn rhema_core::ModelProvider> = config.provider.clone();
        let tools = config.tools.clone().unwrap_or_else(|| {
            Arc::new(ShellTools::with_env(
                config.workspace.clone(),
                &config.shell_env,
            ))
        });
        // The sandbox assembly builds its tools before the wirings add
        // their env; the config's map is the one all five kits share.
        if !config.shell_env.is_empty() {
            tools.set_shell_env(&config.shell_env);
        }
        let resolver: Arc<dyn ToolResolver> = match config.extra_resolver.clone() {
            Some(extra) => Arc::new(Overlay::new(tools.clone(), extra)),
            None => tools.clone(),
        };
        // The transcript: the bounded view rides the provider, `zoom`
        // joins the resolver chain, and the prompt's tool list grows to
        // match, the prompt and the request's tools cannot drift.
        let mut extra: Vec<ToolSpec> = config
            .extra_resolver
            .as_ref()
            .map(|resolver| resolver.specs())
            .unwrap_or_default();
        // A model with no vision gets no pixels: the mask rewrites
        // image blocks into text notes on the request's own copy,
        // after the transcript's view is built and before the wire.
        // The session keeps them, so a rebuild over a vision model
        // sends them again.
        let provider = rhema_core::ImageMask::wrap(provider, config.supports_vision);
        let (provider, resolver, transcript) = match config.transcript {
            Some(settings) => {
                extra.push(rhema_transcript::Zoom::spec());
                let (provider, zoom, mirror) = rhema_transcript::attach_with_summarizer(
                    provider,
                    settings,
                    config.summary_worker.clone(),
                );
                let resolver = Arc::new(Overlay::new(resolver, Arc::new(zoom)));
                (
                    provider,
                    Arc::clone(&resolver) as Arc<dyn ToolResolver>,
                    Some(mirror),
                )
            }
            None => (provider, resolver, None),
        };
        // The skill list: pre-collected or discovered, minus the
        // disabled names. The prompt renders it, and the transcript
        // pins the bodies, the disabled names drop before either sees
        // the list.
        let mut skills = match &config.skills {
            Some(skills) => skills.clone(),
            None => {
                let home = config
                    .home
                    .clone()
                    .or_else(|| rhema_config::Home::discover().ok());
                let agents = home.as_ref().map(|h| h.agents_skills());
                let global = home.as_ref().map(|h| h.skills());
                rhema_skills::discover(agents.as_deref(), global.as_deref(), tools.workspace())
            }
        };
        skills.retain(|skill| !config.disabled_skills.contains(&skill.name));
        // The MCP section honors the same persisted off switch: the
        // wire step already filtered before spawning; this is the
        // prompt-side echo, so a frontend feeding lines directly is
        // still governed by the switch.
        let mcp: Vec<rhema_config::mcp::McpServerLine> = config
            .mcp_servers
            .iter()
            .filter(|line| !config.disabled_mcp_servers.contains(&line.server))
            .cloned()
            .collect();
        let sections = config_sections(&config, &mcp);
        let system = match config.system.clone() {
            // An explicit total replacement is the escape hatch:
            // verbatim, not templated.
            Some(system) => system,
            None => stamped_system_with_sections(
                config.base_system.as_deref(),
                tools.workspace(),
                &extra,
                &skills,
                &sections,
            )?,
        };
        // The skills section's byte share of the system prompt: the
        // same render without the skills list, subtracted. The
        // transcript's usage breakdown splits the prompt on it; a
        // verbatim replacement keeps zero (the split is unknowable).
        let skills_bytes = match config.system {
            Some(_) => 0,
            None => {
                // The subtraction measures the skills section exactly:
                // rendering is deterministic for fixed inputs (skills
                // arrive from BTreeMap-sorted discovery), so the bare
                // re-render differs from `system` by that section alone.
                // The environment and user sections ride both renders
                // identically, they carry no skills, and cancel out.
                stamped_system_with_sections(
                    config.base_system.as_deref(),
                    tools.workspace(),
                    &extra,
                    &[],
                    &sections,
                )
                .map(|without| system.len().saturating_sub(without.len()))
                .unwrap_or(0)
            }
        };
        if let Some(mirror) = &transcript {
            let mut mirror = mirror.lock().expect("transcript lock poisoned");
            mirror.set_skills_bytes(skills_bytes);
            // The bodies pin: a tool result carrying one stays whole in
            // the block, so a compacted skill's rules stay in force.
            mirror.set_skills(
                skills
                    .iter()
                    .map(|skill| rhema_transcript::SkillText {
                        name: skill.name.clone(),
                        body: skill.body.clone(),
                    })
                    .collect(),
            );
        }
        let agent = Agent::builder()
            .provider(provider)
            // The field keeps one side of the arc: revive must see the
            // whole chain, not just the shell tools.
            .resolver(resolver.clone())
            .system(system)
            .params(ChatParams {
                thinking: config.thinking,
                session_id: config.session_id.clone(),
                ..ChatParams::new(config.model.clone())
            })
            .build()?;
        let mut built = Self {
            agent,
            config,
            session: Session::default(),
            tools,
            resolver,
            transcript,
            turns: 0,
        };
        built.stamp_environment();
        Ok(built)
    }

    /// A fresh agent resuming `session` (e.g. from
    /// [`HostAgent::load_session_from`]); the next turn continues the
    /// prior history. The history arrives exactly as the caller loaded
    /// it: a consumer that stores its own display encodings in the
    /// text sanitizes before it calls.
    ///
    /// The conversation's environment report is conversation state: a
    /// session carrying one reuses its bytes verbatim, whatever the
    /// config would render fresh, the report's date names the
    /// conversation's start, and a resume that re-stamped it would
    /// rewrite the system prompt and break the provider's prompt cache
    /// for the whole prefix. A session without one (a conversation from
    /// before the report existed) stamps the config's render and
    /// carries it forward.
    pub fn from_session(
        mut config: HostAgentConfig,
        session: Session,
    ) -> Result<Self, HostAgentError> {
        if let Some(stamped) = session
            .driver_state
            .get(ENVIRONMENT_STATE_KEY)
            .and_then(|value| value.as_str())
            && config.environment.is_some()
        {
            config.environment = Some(stamped.to_owned());
        }
        let mut agent = Self::new(config)?;
        agent.session = session;
        agent.stamp_environment();
        Ok(agent)
    }

    /// Writes the configured environment section into the session's
    /// driver state when none is stamped yet: the checkpoint paths
    /// persist the session whole, so the stamp rides every save and the
    /// next resume reuses it. Idempotent, and silent when no section is
    /// configured (a verbatim prompt or an off switch).
    fn stamp_environment(&mut self) {
        let Some(report) = self.config.environment.clone() else {
            return;
        };
        let state = &mut self.session.driver_state;
        if !state.is_object() {
            if !state.is_null() {
                // Another driver's shape: never clobbered, never
                // extended, the stamp is skipped, not forced.
                return;
            }
            *state = serde_json::json!({});
        }
        if state.get(ENVIRONMENT_STATE_KEY).is_none() {
            state[ENVIRONMENT_STATE_KEY] = serde_json::json!(report);
        }
    }

    /// Installs the per-stream checkpoint hook on the engine (see
    /// [`rhema_core::CommitHook`]); `None` disables it. A driver that
    /// persists the session sets this so a crash mid-turn, inside a
    /// tool call (a committed intent, which the next turn's revive
    /// closes) or mid-stream, first stream included, leaves a resumable
    /// session at the last boundary rather than no record at all.
    pub fn set_commit_hook(&mut self, hook: Option<CommitHook>) {
        self.agent.set_commit_hook(hook);
    }

    /// Replaces the approval hook the following turns install before
    /// stepping. The constructor already takes one; this is for a
    /// driver whose answer channel belongs to a single turn (the TUI
    /// builds a fresh bridge per round) or whose policy changes
    /// between turns.
    pub fn set_approval(&mut self, approval: impl Approval + 'static) {
        self.config.approval = Arc::new(approval);
    }

    /// The conversation so far.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The rendered system prompt this agent stamps into every request,
    /// or `None` when the caller supplied none.
    pub fn system(&self) -> Option<&str> {
        self.agent.system()
    }

    /// The transcript's compaction benchmark snapshot, request
    /// counts, flushes, block size, and the providers' own prompt-cache
    /// hit rate, when the transcript is on. `None` with it off.
    pub fn transcript_stats(&self) -> Option<rhema_transcript::TranscriptStats> {
        self.transcript
            .as_ref()
            .map(|mirror| mirror.lock().expect("transcript lock poisoned").stats())
    }

    /// Folds a saved session's benchmark counters into the transcript
    /// mirror, when the transcript is on: a resumed session's stats
    /// keep reading as one run, pre-resume requests included. No-op
    /// with the transcript off, there is nothing to continue.
    pub fn carry_transcript_stats(&mut self, saved: &rhema_transcript::TranscriptStats) {
        if let Some(mirror) = &self.transcript {
            mirror
                .lock()
                .expect("transcript lock poisoned")
                .carry(saved);
        }
    }

    /// The mirror's committed sections, oldest first: what a session
    /// save carries so the next session adopts the compaction instead
    /// of re-deriving it and re-paying the worker. Empty with the
    /// transcript off.
    pub fn transcript_sections(&self) -> Vec<rhema_transcript::Section> {
        match &self.transcript {
            Some(mirror) => mirror
                .lock()
                .expect("transcript lock poisoned")
                .saved_sections(),
            None => Vec::new(),
        }
    }

    /// Hands a saved session's sections to the mirror. They wait for
    /// the first request, which adopts them when they tile the
    /// resumed log and drops them for any rebuild. No-op with the
    /// transcript off.
    pub fn adopt_transcript_sections(&mut self, sections: Vec<rhema_transcript::Section>) {
        if let Some(mirror) = &self.transcript {
            mirror
                .lock()
                .expect("transcript lock poisoned")
                .adopt_sections(sections);
        }
    }

    /// Zeroes the transcript's benchmark counters, when the transcript
    /// is on: a conversation cleared mid-agent starts its accounting
    /// over, and the stats describe one conversation, not the agent's
    /// lifetime.
    pub fn reset_transcript_stats(&mut self) {
        if let Some(mirror) = &self.transcript {
            mirror
                .lock()
                .expect("transcript lock poisoned")
                .reset_stats();
        }
    }

    /// Mutable access between turns; the engine owns the history only
    /// inside a turn.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// One turn: push the user input, loop steps until the model stops
    /// requesting tools (capped at `max_turn_steps`; a cap hit returns
    /// the last output with [`StopReason::ToolUse`]), with the config's
    /// approval hook gating the side-effecting tools. Appends to the
    /// session in place; the hook is cleared on the way out, success or
    /// failure. A history entering with unanswered calls, a crash
    /// resume, revives first, so the model sees a closed round, not a
    /// dangling one.
    pub async fn turn(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, HostAgentError> {
        self.turn_with_cancel(input, sink, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// [`Self::turn`] with a driver-held cancel flag: flipping
    /// `cancelled` mid-turn asks in-flight tools to stop (the executor
    /// polls it) and the next resolution to refuse. A driver whose stop
    /// arrives from outside the turn, a frontend's stop message, a
    /// disconnected socket, holds one flag per turn and passes it here;
    /// the plain [`Self::turn`] builds a flag nobody else can reach.
    pub async fn turn_with_cancel(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, HostAgentError> {
        self.turn_message_with_cancel(Message::text(Role::User, input), sink, cancelled)
            .await
    }

    /// [`Self::turn`] over a whole [`Message`]: the entry point for
    /// inputs carrying image blocks beside the text. Steering stays
    /// text, a steered nudge is a sentence, not a screenshot.
    pub async fn turn_message(
        &mut self,
        message: Message,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, HostAgentError> {
        self.turn_message_with_cancel(message, sink, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// [`Self::turn_message`] with [`Self::turn_with_cancel`]'s cancel
    /// flag, the full form both convenience wrappers fold into.
    pub async fn turn_message_with_cancel(
        &mut self,
        message: Message,
        sink: &mut dyn EventSink,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, HostAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled,
        };
        // The hook goes on before revive: a replay-safe call re-runs
        // here, and it must do so under this turn's approval, not with
        // no hook answering "rejected" for a prompt nobody saw.
        self.tools.install_approval(self.config.approval.clone());
        revive_dangling_calls(&mut self.session, self.resolver.as_ref(), &ctx).await;
        // With a steering source the loop is this driver's own, the
        // same shape as the provided `run_turn` plus the two drain
        // points; without one, the provided convenience runs unchanged.
        let result = match self.config.steering.clone() {
            Some(steering) => {
                self.steps(Some(message), sink, &ctx, Some(steering.as_ref()))
                    .await
            }
            None => {
                run_turn(
                    &mut self.agent,
                    &mut self.session,
                    message,
                    sink,
                    &ctx,
                    self.config.max_turn_steps,
                )
                .await
            }
        };
        self.tools.clear_approval();
        Ok(result?)
    }

    /// The failed request again: no new input message, the step loop
    /// runs on the history exactly as the failed turn left it, so the
    /// same request goes out. A first-request failure re-issues the
    /// identical request; a mid-turn failure re-issues the step that
    /// failed, with the committed steps still in view (the model sees
    /// what already ran and continues from there, no committed tool
    /// call re-executes behind the model's back). A history entering
    /// with unanswered calls, a commit-hook failure after an intent
    /// landed, revives first, like any turn. Approval comes from the
    /// config and cancel wires in as in [`Self::turn_message_with_cancel`].
    pub async fn retry_turn_with_cancel(
        &mut self,
        sink: &mut dyn EventSink,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, HostAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled,
        };
        // The hook goes on before revive: a replay-safe call re-runs
        // here, and it must do so under this turn's approval, not with
        // no hook answering "rejected" for a prompt nobody saw.
        self.tools.install_approval(self.config.approval.clone());
        revive_dangling_calls(&mut self.session, self.resolver.as_ref(), &ctx).await;
        let steering = self.config.steering.clone();
        let result = self.steps(None, sink, &ctx, steering.as_deref()).await;
        self.tools.clear_approval();
        Ok(result?)
    }

    /// The driver's own step loop: [`run_turn`]'s shape, generalized
    /// over the two axes the turn shapes differ on, whether a new
    /// input message is pushed (a retry pushes none; it re-requests on
    /// the history as the last turn left it) and whether a steering
    /// source drains between steps. With an input and no steering the
    /// provided [`run_turn`] runs instead; this loop covers the shapes
    /// it does not. Anything finished while no turn ran is announced at
    /// loop start, before the input (consecutive User messages are
    /// valid); between steps, what finished during the last one is
    /// announced before the next. The append lands at a step boundary,
    /// so the next commit hook persists it like any history.
    async fn steps(
        &mut self,
        input: Option<Message>,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
        steering: Option<&dyn Steering>,
    ) -> Result<ModelOutput, AgentError> {
        // Fresh per turn: a finish announced in an earlier turn is
        // announced again here while its report stays unfetched.
        let mut announced: Vec<String> = Vec::new();
        if let Some(steering) = steering {
            self.append_notifications(steering, &mut announced);
        }
        if let Some(input) = input {
            self.session.history.push(input);
        }
        let mut step = self.agent.step(&mut self.session, sink, ctx).await?;
        let mut remaining = self.config.max_turn_steps.get() - 1;
        while step.output.stop_reason == StopReason::ToolUse {
            if remaining == 0 {
                break;
            }
            if let Some(steering) = steering {
                self.append_notifications(steering, &mut announced);
            }
            step = self.agent.step(&mut self.session, sink, ctx).await?;
            remaining -= 1;
        }
        Ok(step.output)
    }

    /// Drains the steering source and appends what is new as one
    /// User-role message, one line per notification. The drain is a pure
    /// query, so the same line can come back on the next drain, within
    /// a turn each finish is announced once, and `announced` carries the
    /// lines already appended to this turn's history.
    fn append_notifications(&mut self, steering: &dyn Steering, announced: &mut Vec<String>) {
        let lines: Vec<String> = steering
            .drain()
            .into_iter()
            .filter(|line| !announced.contains(line))
            .collect();
        if lines.is_empty() {
            return;
        }
        announced.extend(lines.iter().cloned());
        self.session
            .history
            .push(Message::text(Role::User, lines.join("\n")));
    }

    /// Writes the session as pretty JSON to `path` through
    /// [`rhema_session_store_file::PathStore`]: `0600`, landed through a
    /// private temp file, fsynced, and renamed, so nothing
    /// conversation-shaped is ever world-readable and a crash mid-save
    /// leaves the previous checkpoint whole.
    pub fn save_session_to(&self, path: &impl AsRef<Path>) -> io::Result<()> {
        PathStore::new(path.as_ref())
            .save(&self.session)
            .map_err(store_to_io)
    }

    /// Reads a session written by [`HostAgent::save_session_to`]. A
    /// missing path keeps the `NotFound` kind the callers' "cannot
    /// resume" wording reads.
    pub fn load_session_from(path: &impl AsRef<Path>) -> io::Result<Session> {
        let path = path.as_ref();
        PathStore::new(path)
            .load()
            .map_err(store_to_io)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no session file at {}", path.display()),
                )
            })
    }
}

/// The store's failures as the `io::Result` this driver's public
/// session helpers have always returned: a real IO failure keeps its
/// kind, everything else reads as invalid data.
fn store_to_io(err: StoreError) -> io::Error {
    match err {
        StoreError::Io { source, .. } => source,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod provider_boundary_tests {
    use super::*;
    use rhema_bash::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, Usage, async_trait,
    };
    use std::sync::Mutex;

    /// Records the user text each request carried, then ends the turn.
    struct Recording(Mutex<Vec<String>>);

    #[async_trait]
    impl ModelProvider for Recording {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let mut user = String::new();
            for message in &req.messages {
                if message.role != Role::User {
                    continue;
                }
                for content in &message.content {
                    if let Content::Text(text) = content {
                        user.push_str(text);
                    }
                }
            }
            self.0.lock().unwrap().push(user);
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    #[tokio::test]
    async fn an_annotated_prompt_reaches_the_provider_plain() {
        let text = "look at l1\nl2 this";
        let mut message = Message::text(Role::User, text.to_owned());
        message.annotate(
            "ui",
            serde_json::json!({ "spans": [{ "start": 8, "end": 13 }] }),
        );
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let mut agent = HostAgent::new(HostAgentConfig::new(
            recording.clone(),
            "m",
            std::env::temp_dir(),
            AllowAll,
        ))
        .unwrap();
        agent
            .turn_message(message.clone(), &mut NoopSink)
            .await
            .unwrap();

        // The session keeps the plain text and the caller's note.
        assert_eq!(agent.session().history[0], message);
        assert!(agent.session().history[0].annotation("ui").is_some());
        // The provider sees the same plain text, payload and all.
        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], text);
    }

    /// Records the content blocks of every User message each request
    /// carried, then ends the turn.
    struct RecordingBlocks(Mutex<Vec<Vec<Content>>>);

    #[async_trait]
    impl ModelProvider for RecordingBlocks {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let mut blocks = Vec::new();
            for message in &req.messages {
                if message.role == Role::User {
                    blocks.extend(message.content.iter().cloned());
                }
            }
            self.0.lock().unwrap().push(blocks);
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn blocks_agent(recording: &Arc<RecordingBlocks>) -> HostAgent {
        HostAgent::new(HostAgentConfig::new(
            recording.clone(),
            "m",
            std::env::temp_dir(),
            AllowAll,
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn turn_message_delivers_text_then_image_blocks_to_the_provider() {
        let recording = Arc::new(RecordingBlocks(Mutex::new(Vec::new())));
        let mut agent = blocks_agent(&recording);
        let message = Message::text_with_images(
            Role::User,
            "what is in this picture?",
            vec![
                rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8="),
                rhema_core::Image::new(rhema_core::ImageMediaType::Jpeg, "d29ybGQ="),
            ],
        );
        agent.turn_message(message, &mut NoopSink).await.unwrap();

        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            vec![
                Content::Text("what is in this picture?".into()),
                Content::Image(rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "aGVsbG8="
                )),
                Content::Image(rhema_core::Image::new(
                    rhema_core::ImageMediaType::Jpeg,
                    "d29ybGQ="
                )),
            ]
        );
        // The session history is the message, verbatim, checkpointing
        // must carry the images byte-for-byte.
        assert_eq!(agent.session().history[0].content, seen[0]);
    }

    #[tokio::test]
    async fn a_model_without_vision_gets_notes_instead_of_image_blocks() {
        let recording = Arc::new(RecordingBlocks(Mutex::new(Vec::new())));
        let mut agent = HostAgent::new(HostAgentConfig {
            supports_vision: Some(false),
            ..HostAgentConfig::new(recording.clone(), "m", std::env::temp_dir(), AllowAll)
        })
        .unwrap();
        let message = Message::text_with_images(
            Role::User,
            "what is in this picture?",
            vec![rhema_core::Image::new(
                rhema_core::ImageMediaType::Png,
                "aGVsbG8=",
            )],
        );
        agent.turn_message(message, &mut NoopSink).await.unwrap();

        let seen = recording.0.lock().unwrap();
        assert_eq!(
            seen[0],
            vec![
                Content::Text("what is in this picture?".into()),
                Content::Text("[image omitted: image/png; the active model has no vision]".into()),
            ]
        );
        // The log kept the pixels: the mask is a request projection,
        // and a later vision model sends them again.
        assert!(
            agent.session().history[0]
                .content
                .iter()
                .any(|block| matches!(block, Content::Image(_)))
        );
    }

    #[tokio::test]
    async fn the_text_entry_still_sends_exactly_one_text_block() {
        let recording = Arc::new(RecordingBlocks(Mutex::new(Vec::new())));
        let mut agent = blocks_agent(&recording);
        agent.turn("plain words", &mut NoopSink).await.unwrap();

        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], vec![Content::Text("plain words".into())]);
    }
}

#[cfg(test)]
mod extra_tools_tests {
    use super::*;
    use rhema_bash::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, ToolCall,
        ToolResult, Usage, async_trait,
    };
    use rhema_prompt::SkillTier;
    use std::sync::Mutex;

    use serde_json::json;

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    /// An extension tool: `echo` answers with its own `text` argument.
    struct Echo;

    #[async_trait]
    impl rhema_core::ToolResolver for Echo {
        fn specs(&self) -> Vec<rhema_core::ToolSpec> {
            vec![rhema_core::ToolSpec::new(
                "echo",
                "Repeats the text argument back. The extension's canary.",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            )]
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            let text = call.args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ToolResult::success(call.id, format!("echo: {text}"))
        }
    }

    /// Scripts one tool-use round (calling `echo`) followed by an
    /// end-turn; records the tool names every request advertised.
    struct Scripted(Mutex<Vec<Vec<String>>>);

    #[async_trait]
    impl ModelProvider for Scripted {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let round = self.0.lock().unwrap().len();
            self.0
                .lock()
                .unwrap()
                .push(req.tools.iter().map(|t| t.name.clone()).collect());
            if round == 0 {
                Ok(ModelOutput {
                    message: Message {
                        annotations: Default::default(),
                        role: Role::Assistant,
                        content: vec![Content::ToolCall(ToolCall {
                            id: "c1".into(),
                            name: "echo".into(),
                            args: json!({"text": "through the overlay"}),
                            ..ToolCall::default()
                        })],
                    },
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                })
            } else {
                Ok(ModelOutput {
                    message: Message::text(Role::Assistant, "final"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }
    }

    #[tokio::test]
    async fn the_extra_resolver_reaches_the_request_and_resolves() {
        let scripted = Arc::new(Scripted(Mutex::new(Vec::new())));
        let config = HostAgentConfig {
            extra_resolver: Some(Arc::new(Echo)),
            ..HostAgentConfig::new(scripted.clone(), "m", std::env::temp_dir(), AllowAll)
        };
        let mut agent = HostAgent::new(config).unwrap();
        let output = agent.turn("use the tool", &mut NoopSink).await.unwrap();

        // The extension tool resolved in-band and its result reached the
        // model, which then ended the turn.
        let text: String = output
            .message
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "final");
        let rounds = scripted.0.lock().unwrap();
        assert_eq!(rounds.len(), 2);
        assert!(
            rounds[0].contains(&"echo".to_owned()) && rounds[0].contains(&"bash".to_owned()),
            "the tools array carries both: {:?}",
            rounds[0]
        );
        // The prompt names the extension tool too: prompt and tools
        // array cannot drift.
        let prompt = agent
            .system()
            .expect("the agent was built with a system prompt");
        assert!(prompt.contains("echo"), "{prompt}");
        assert!(
            prompt.contains("canary"),
            "the extension's description is in the prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn the_default_advertises_zoom_and_none_skips_it() {
        // Default: the built-ins plus zoom, the prompt naming it too.
        let scripted = Arc::new(Scripted(Mutex::new(Vec::new())));
        let mut agent = HostAgent::new(HostAgentConfig::new(
            scripted.clone(),
            "m",
            std::env::temp_dir(),
            AllowAll,
        ))
        .unwrap();
        agent.turn("hi", &mut NoopSink).await.unwrap();
        {
            let rounds = scripted.0.lock().unwrap();
            assert_eq!(rounds[0], vec!["bash".to_owned(), "zoom".to_owned()]);
        }
        let prompt = agent
            .system()
            .expect("the agent was built with a system prompt");
        assert!(prompt.contains("zoom"), "{prompt}");
        assert!(
            !agent
                .system()
                .is_some_and(|prompt| prompt.contains("canary"))
        );

        // `transcript: None` is the off switch: the built-ins alone.
        let scripted = Arc::new(Scripted(Mutex::new(Vec::new())));
        let config = HostAgentConfig {
            transcript: None,
            ..HostAgentConfig::new(scripted.clone(), "m", std::env::temp_dir(), AllowAll)
        };
        let mut agent = HostAgent::new(config).unwrap();
        agent.turn("hi", &mut NoopSink).await.unwrap();
        let rounds = scripted.0.lock().unwrap();
        assert_eq!(rounds[0], vec!["bash".to_owned()]);
        let prompt = agent
            .system()
            .expect("the agent was built with a system prompt");
        assert!(!prompt.contains("zoom"), "{prompt}");
        assert!(
            !agent
                .system()
                .is_some_and(|prompt| prompt.contains("canary"))
        );
    }

    #[test]
    fn pre_collected_skills_reach_the_prompt_without_a_filesystem() {
        let workspace = std::env::temp_dir();
        let skill = Skill {
            name: "deploy".into(),
            description: "Triggers on deploy tasks.".into(),
            license: None,
            path: workspace.join(".rhema/skills/deploy/SKILL.md"),
            tier: SkillTier::Workspace,
            body: "the deploy rules".into(),
        };
        let prompt = stamped_system_with_skills(None, &workspace, &[], &[skill]).unwrap();
        assert!(prompt.contains("deploy"), "{prompt}");
        assert!(prompt.contains("Triggers on deploy tasks."), "{prompt}");
        // The discovery default is untouched: no such skill on disk.
        let without = stamped_system(None, &workspace, &[]).unwrap();
        assert!(!without.contains("Triggers on deploy tasks."), "{without}");
    }

    #[test]
    fn stamped_system_carries_the_extra_specs() {
        let workspace = std::env::temp_dir();
        let spec = rhema_core::ToolSpec::new(
            "echo",
            "Repeats text back. The extension's canary.",
            json!({"type": "object"}),
        );
        let with = stamped_system(None, &workspace, std::slice::from_ref(&spec)).unwrap();
        assert!(with.contains("echo") && with.contains("canary"), "{with}");
        let without = stamped_system(None, &workspace, &[]).unwrap();
        assert!(!without.contains("canary"), "{without}");
    }

    #[test]
    fn the_optional_sections_reach_the_prompt_where_the_template_places_them() {
        let workspace = std::env::temp_dir();
        let sections = vec![
            (
                "environment",
                serde_json::json!("## Environment\n\n- OS: test os"),
            ),
            ("user", serde_json::json!("prefer concise answers")),
            ("vision", serde_json::json!("pipe bytes into `see`")),
            ("jobs", serde_json::json!("`job run` starts it")),
            (
                "hear",
                serde_json::json!("`hear clip.mp3` attaches the clip"),
            ),
        ];
        let prompt = stamped_system_with_sections(None, &workspace, &[], &[], &sections).unwrap();
        assert!(prompt.contains("- OS: test os"), "{prompt}");
        assert!(prompt.contains("prefer concise answers"), "{prompt}");
        assert!(prompt.contains("pipe bytes into `see`"), "{prompt}");
        assert!(prompt.contains("`job run` starts it"), "{prompt}");
        // The section-less constructors render neither: an absent
        // section is the render a template without the blocks makes.
        let bare = stamped_system_with_skills(None, &workspace, &[], &[]).unwrap();
        assert!(!bare.contains("- OS: test os"), "{bare}");
        assert!(!bare.contains("`job run` starts it"), "{bare}");
        assert!(!bare.contains("prefer concise answers"), "{bare}");
        assert!(!bare.contains("pipe bytes into `see`"), "{bare}");
    }

    /// The reason sections are a list: a frontend's own macro needs no
    /// new parameter and no new config field order, only its name.
    #[test]
    fn a_section_the_crate_never_names_reaches_the_prompt() {
        let sections = vec![(
            "hear",
            serde_json::json!("`hear clip.mp3` attaches the clip as audio"),
        )];
        let prompt = stamped_system_with_sections(
            Some("base\n{% if hear %}Audio:\n{{ hear }}\n{% endif %}"),
            &std::env::temp_dir(),
            &[],
            &[],
            &sections,
        )
        .unwrap();
        assert!(
            prompt.contains("Audio:\n`hear clip.mp3` attaches the clip as audio"),
            "{prompt}"
        );
    }

    /// Every section the config carries is a section the prompt renders.
    /// `websearch` used to be the counter-example: the config set it and
    /// the render call had no argument for it, so the `search` macro was
    /// never advertised. One list fixed the class, not the instance.
    #[test]
    fn the_config_folds_every_section_it_carries_into_one_list() {
        let config = HostAgentConfig {
            environment: Some("env".to_owned()),
            user_preferences: Some("prefs".to_owned()),
            vision: Some("see".to_owned()),
            jobs: Some("job".to_owned()),
            websearch: Some("search".to_owned()),
            decision: Some("decide".to_owned()),
            sections: std::collections::BTreeMap::from([(
                "hear".to_owned(),
                serde_json::json!("hear"),
            )]),
            ..HostAgentConfig::new(
                Scripted(Mutex::new(Vec::new())),
                "m",
                std::env::temp_dir(),
                AllowAll,
            )
        };
        let lines = vec![rhema_config::mcp::McpServerLine {
            name: "atlassian".into(),
            server: "atlassian".into(),
            description: "Jira".into(),
        }];
        let names: Vec<&str> = config_sections(&config, &lines)
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            names,
            [
                "environment",
                "user",
                "vision",
                "jobs",
                "websearch",
                "decision",
                "mcp",
                "hear"
            ]
        );
        // End to end through the built-in template: the section the
        // config set is the section the prompt renders.
        let sections = config_sections(&config, &lines);
        let prompt =
            stamped_system_with_sections(None, &std::env::temp_dir(), &[], &[], &sections).unwrap();
        assert!(prompt.contains("## Web search"), "{prompt}");
        assert!(prompt.contains("## Decision models"), "{prompt}");
    }
}

#[cfg(test)]
mod environment_stamp_tests {
    use super::*;
    use rhema_bash::AllowAll;
    use rhema_core::{
        Event, ModelProvider, ModelRequest, ProviderError, StopReason, Usage, async_trait,
    };

    /// Ends the turn at once; the exchange never matters here.
    struct Ender;
    #[async_trait]
    impl ModelProvider for Ender {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    struct NoopSink;
    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    fn config(provider: Arc<dyn ModelProvider>, report: &str) -> HostAgentConfig {
        HostAgentConfig {
            environment: Some(report.to_owned()),
            ..HostAgentConfig::new(provider, "m", std::env::temp_dir(), AllowAll)
        }
    }

    /// A fresh conversation stamps its report at construction, before
    /// any turn: the first checkpoint already carries it.
    #[test]
    fn a_fresh_conversation_stamps_at_construction() {
        let agent = HostAgent::new(config(Arc::new(Ender), "REPORT-A")).unwrap();
        assert_eq!(
            agent
                .session()
                .driver_state
                .get(ENVIRONMENT_STATE_KEY)
                .and_then(|v| v.as_str()),
            Some("REPORT-A")
        );
    }

    /// The resume contract: a session carrying a report reuses its
    /// bytes, whatever the config would render fresh, the date inside
    /// names the conversation's start, and a re-stamp would rewrite
    /// the system prompt and break the prefix cache.
    #[tokio::test]
    async fn a_resumed_conversation_keeps_its_stamped_report() {
        let mut first = HostAgent::new(config(Arc::new(Ender), "REPORT-A")).unwrap();
        first.turn("hi", &mut NoopSink).await.unwrap();
        let session = first.session().clone();

        // Whatever the rebuilt config carries, a model switch, a
        // profile switch, a resume, the conversation's bytes stand.
        let second = HostAgent::from_session(config(Arc::new(Ender), "REPORT-B"), session).unwrap();
        let prompt = second.system().expect("the agent has a prompt");
        assert!(prompt.contains("REPORT-A"), "{prompt}");
        assert!(!prompt.contains("REPORT-B"), "{prompt}");
        assert_eq!(
            second
                .session()
                .driver_state
                .get(ENVIRONMENT_STATE_KEY)
                .and_then(|v| v.as_str()),
            Some("REPORT-A"),
            "the stamp is idempotent across rebuilds"
        );
    }

    /// A conversation from before the report existed stamps the fresh
    /// render and carries it forward from there.
    #[tokio::test]
    async fn a_pre_report_session_stamps_the_fresh_render_once() {
        let mut first = HostAgent::new(config(Arc::new(Ender), "REPORT-A")).unwrap();
        first.turn("hi", &mut NoopSink).await.unwrap();
        // Age the session: the stamp disappears, the history stays.
        let session = Session {
            driver_state: serde_json::Value::Null,
            ..first.session().clone()
        };

        let second = HostAgent::from_session(config(Arc::new(Ender), "REPORT-B"), session).unwrap();
        let prompt = second.system().expect("the agent has a prompt");
        assert!(prompt.contains("REPORT-B"), "{prompt}");
        assert_eq!(
            second
                .session()
                .driver_state
                .get(ENVIRONMENT_STATE_KEY)
                .and_then(|v| v.as_str()),
            Some("REPORT-B"),
            "stamped once on arrival, carried forward"
        );
    }

    /// Another driver's keys coexist with the stamp, driver_state is
    /// one object per driver key, and a non-object shape is never
    /// clobbered: no stamp, no error, the section still renders.
    #[test]
    fn driver_state_shapes_coexist_or_stay_untouched() {
        let session = Session {
            driver_state: serde_json::json!({ "someone-elses": "shape" }),
            ..Session::default()
        };
        let agent = HostAgent::from_session(config(Arc::new(Ender), "REPORT-A"), session).unwrap();
        let state = &agent.session().driver_state;
        assert_eq!(
            state.get("someone-elses"),
            Some(&serde_json::json!("shape")),
            "the other driver's key stands"
        );
        assert_eq!(
            state.get(ENVIRONMENT_STATE_KEY).and_then(|v| v.as_str()),
            Some("REPORT-A"),
            "the stamp coexists beside it"
        );

        let opaque = Session {
            driver_state: serde_json::json!("a driver's opaque shape"),
            ..Session::default()
        };
        let agent = HostAgent::from_session(config(Arc::new(Ender), "REPORT-A"), opaque).unwrap();
        assert_eq!(
            agent.session().driver_state,
            serde_json::json!("a driver's opaque shape"),
            "a non-object shape is never clobbered"
        );
        assert!(
            agent
                .system()
                .is_some_and(|prompt| prompt.contains("REPORT-A")),
            "the section still renders for this conversation"
        );
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use rhema_bash::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, ToolCall,
        ToolResult, Usage, async_trait,
    };
    use std::sync::Mutex;

    /// Records every request's message list, then ends the turn.
    struct Recording(Mutex<Vec<Vec<Message>>>);

    #[async_trait]
    impl ModelProvider for Recording {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            self.0.lock().unwrap().push(req.messages);
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    /// A history over any small budget: user, then big tool rounds.
    fn big_history(rounds: usize) -> Session {
        let mut history = vec![Message::text(Role::User, "fix the build")];
        for i in 0..rounds {
            history.push(Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![
                    Content::Text(format!("round {i}")),
                    Content::ToolCall(ToolCall {
                        id: format!("c{i}"),
                        name: "bash".into(),
                        args: serde_json::json!({ "command": "make" }),
                        provider_data: serde_json::Value::Null,
                    }),
                ],
            });
            history.push(Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::success(
                    format!("c{i}"),
                    "x".repeat(2048),
                ))],
            });
        }
        Session {
            history,
            ..Session::default()
        }
    }

    #[tokio::test]
    async fn off_the_history_rides_verbatim() {
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let config = HostAgentConfig {
            transcript: None,
            ..HostAgentConfig::new(recording.clone(), "m", std::env::temp_dir(), AllowAll)
        };
        let mut agent = HostAgent::from_session(config, big_history(5)).unwrap();
        agent.turn("next", &mut NoopSink).await.unwrap();
        let sent = recording.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // Restored history plus this turn's input, nothing rewritten.
        assert_eq!(sent[0].len(), 5 * 2 + 2);
        assert_eq!(sent[0][0], Message::text(Role::User, "fix the build"));
    }

    #[tokio::test]
    async fn on_by_default_a_big_history_is_viewed_and_the_session_stays_whole() {
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let config = HostAgentConfig {
            transcript: Some(rhema_transcript::TranscriptSettings {
                max_view_bytes: 4_000,
                ..rhema_transcript::TranscriptSettings::default()
            }),
            ..HostAgentConfig::new(recording.clone(), "m", std::env::temp_dir(), AllowAll)
        };
        let mut agent = HostAgent::from_session(config, big_history(5)).unwrap();
        // The prompt advertises zoom: the tool and the prompt cannot drift.
        let prompt = agent
            .system()
            .expect("the agent was built with a system prompt");
        assert!(prompt.contains("zoom"), "{prompt}");
        agent.turn("next", &mut NoopSink).await.unwrap();

        let sent = recording.0.lock().unwrap();
        let messages = &sent[0];
        assert!(
            messages.len() < 5 * 2 + 2,
            "the view is shorter: {} messages",
            messages.len()
        );
        let rhema_core::Content::Text(text) = &messages[0].content[0] else {
            panic!("a text view message");
        };
        assert!(text.starts_with("<transcript>"), "{text}");
        assert_eq!(messages.last().unwrap(), &Message::text(Role::User, "next"));

        // The session kept every message, restored, this turn's
        // input, and the turn's reply, the log is untouched.
        assert_eq!(agent.session().history.len(), 5 * 2 + 3);
    }
}
