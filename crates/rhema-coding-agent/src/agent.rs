//! The coding-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop — the provided `rhema_core::run_turn`, or, with a
//! steering source configured, the driver's own loop with the two drain
//! points that append finished-work notifications at step boundaries.
//!
//! The agent owns one [`ShellTools`] resolver for its lifetime. Approval
//! arrives per turn; it is installed into the resolver before stepping and
//! cleared after (see [`rhema_agent_tools::approval`] for why the hook travels
//! as an `Arc`). The session is plain data — the driver may mutate it between
//! turns, checkpoint it, or resume it from disk. A turn entering with
//! committed-but-unanswered calls (a crash resume) revives first:
//! replay-safe tools re-run, the rest are reported interrupted — see
//! [`revive_dangling_calls`].

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use rhema_agent_tools::{
    Approval, ExpandPastes, Overlay, ShellTools, Skill, revive_dangling_calls,
};
use rhema_core::{
    Agent, AgentError, ChatParams, CommitHook, EventSink, Message, ModelOutput, Role, Session,
    StopReason, ToolContext, ToolResolver, ToolSpec, run_turn,
};

use crate::config::{CodingAgentConfig, Steering};

/// The driver-state key carrying the conversation's rendered
/// environment section: stamped at the conversation's start, reused by
/// every resume, so the system prompt's bytes — and with them the
/// provider's prompt cache for the whole prefix — survive rebuilds.
const ENVIRONMENT_STATE_KEY: &str = "environment_report";

/// Clears a session's stamped environment report — the transition hook
/// for rebuilds that change the facts the report carries: a machine
/// switch (a sandbox picked mid-conversation re-reports where `bash`
/// runs) or a model switch (the report names the model, and the
/// provider's cache is per-model regardless). Transitions that change
/// neither — a thinking level, a resume — keep the stamp: the report is
/// conversation state, and re-rendering it would rewrite the system
/// prompt and break the prefix cache for nothing.
pub fn clear_environment_stamp(session: &mut Session) {
    if let Some(state) = session.driver_state.as_object_mut() {
        state.remove(ENVIRONMENT_STATE_KEY);
    }
}

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
    /// The system-prompt template failed to render — bad syntax, unknown
    /// filter — with the template position when known. The agent refuses
    /// to start on a prompt it cannot fully render.
    #[error("system prompt template: {0}")]
    Template(String),
}

impl From<minijinja::Error> for CodingAgentError {
    fn from(err: minijinja::Error) -> Self {
        let line = err
            .line()
            .map(|line| format!(", line {line}"))
            .unwrap_or_default();
        Self::Template(format!("{err}{line}"))
    }
}

/// The system prompt an agent stamps into every request, assembled
/// outside an agent: `base` — the user's SYSTEM.md — when given, the
/// built-in default otherwise, rendered over `workspace` as given, the
/// discovered skills, and the tool specs — the built-ins plus
/// `extra`, the extension resolver's specs, so the prompt and the
/// request's tools array cannot drift. The one assembly point:
/// [`CodingAgent::new`] builds every agent's prompt here, and a driver
/// that previews the prompt calls the same function, so what is
/// previewed is what turns send. No optional sections: the environment
/// report and the user preferences render empty.
pub fn stamped_system(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
) -> Result<String, CodingAgentError> {
    // Either global directory may be missing (no HOME) — as normal as
    // an empty one.
    let agents = rhema_config::Config::agents_skills_dir().ok();
    let global = rhema_config::Config::skills_dir().ok();
    let skills =
        rhema_agent_tools::skills::discover(agents.as_deref(), global.as_deref(), workspace);
    stamped_system_with_sections(base, workspace, extra, &skills, None, None, &[], None)
}

/// [`stamped_system`] over skills collected elsewhere — the input path
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
) -> Result<String, CodingAgentError> {
    stamped_system_with_sections(base, workspace, extra, skills, None, None, &[], None)
}

/// The full assembly: the template over `workspace`, the skills, the
/// tool specs, and the two optional sections — `environment` (machine
/// facts probed where commands run, see [`crate::environment`]) and
/// `user` (the user's preferences file, verbatim). The template decides
/// whether and where the sections appear; `None` is the section-less
/// render the older constructors name.
#[expect(
    clippy::too_many_arguments,
    reason = "one parameter per prompt section keeps the render's inputs flat"
)]
pub fn stamped_system_with_sections(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
    skills: &[Skill],
    environment: Option<&str>,
    user: Option<&str>,
    mcp: &[rhema_agent_tools::prompt::McpServerLine],
    vision: Option<&str>,
) -> Result<String, CodingAgentError> {
    let mut tools = ShellTools::builtin_specs();
    tools.extend_from_slice(extra);
    rhema_agent_tools::render_system_with_sections(
        base.unwrap_or(crate::prompt::DEFAULT_SYSTEM_MD),
        workspace,
        skills,
        &tools,
        environment,
        user,
        mcp,
        vision,
    )
    .map_err(CodingAgentError::from)
}

/// A coding agent: drives [`run_turn`] over the built-in tools, one turn at
/// a time, keeping the history in a caller-visible [`Session`].
pub struct CodingAgent {
    agent: Agent,
    config: CodingAgentConfig,
    session: Session,
    tools: Arc<ShellTools>,
    /// The composed resolver — shell tools plus every extension (zoom,
    /// subagents). Revive classifies dangling calls against it, so an
    /// extension tool's replay-safety answer is honored; `tools` alone
    /// would answer `Unsafe` for every name.
    resolver: Arc<dyn ToolResolver>,
    /// The transcript's shared mirror, when the config enables it: the
    /// benchmark stats read from here.
    transcript: Option<Arc<Mutex<rhema_transcript::Transcript>>>,
    turns: u64,
}

impl CodingAgent {
    /// A fresh agent with an empty session. The workspace is canonicalized
    /// for tool scoping; the system prompt defaults to the built-in
    /// template rendered by [`crate::prompt::render_system`] unless
    /// `config.system` replaces it. A base prompt that fails to render
    /// fails the construction.
    pub fn new(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
    ) -> Result<Self, CodingAgentError> {
        // The paste expansion rides the provider seam, not the engine:
        // the session keeps the compact form, and this one wrapper is
        // every request path's way out.
        let provider: Arc<dyn rhema_core::ModelProvider> = Arc::new(ExpandPastes::new(provider));
        let tools = config.tools.clone().unwrap_or_else(|| {
            Arc::new(ShellTools::with_env(
                config.workspace.clone(),
                &config.shell_env,
            ))
        });
        let resolver: Arc<dyn ToolResolver> = match config.extra_resolver.clone() {
            Some(extra) => Arc::new(Overlay::new(tools.clone(), extra)),
            None => tools.clone(),
        };
        // The transcript: the bounded view rides the provider, `zoom`
        // joins the resolver chain, and the prompt's tool list grows to
        // match — the prompt and the request's tools cannot drift.
        let mut extra: Vec<ToolSpec> = config
            .extra_resolver
            .as_ref()
            .map(|resolver| resolver.specs())
            .unwrap_or_default();
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
        // pins the bodies — the disabled names drop before either sees
        // the list.
        let mut skills = match &config.skills {
            Some(skills) => skills.clone(),
            None => {
                let agents = rhema_config::Config::agents_skills_dir().ok();
                let global = rhema_config::Config::skills_dir().ok();
                rhema_agent_tools::skills::discover(
                    agents.as_deref(),
                    global.as_deref(),
                    tools.workspace(),
                )
            }
        };
        skills.retain(|skill| !config.disabled_skills.contains(&skill.name));
        // The MCP section honors the same persisted off switch: the
        // wire step already filtered before spawning; this is the
        // prompt-side echo, so a frontend feeding lines directly is
        // still governed by the switch.
        let mcp: Vec<rhema_agent_tools::prompt::McpServerLine> = config
            .mcp_servers
            .iter()
            .filter(|line| !config.disabled_mcp_servers.contains(&line.server))
            .cloned()
            .collect();
        let system = match config.system.clone() {
            // An explicit total replacement is the escape hatch:
            // verbatim, not templated.
            Some(system) => system,
            None => stamped_system_with_sections(
                config.base_system.as_deref(),
                tools.workspace(),
                &extra,
                &skills,
                config.environment.as_deref(),
                config.user_preferences.as_deref(),
                &mcp,
                config.vision.as_deref(),
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
                // identically — they carry no skills — and cancel out.
                let mut bare = ShellTools::builtin_specs();
                bare.extend_from_slice(&extra);
                rhema_agent_tools::render_system_with_sections(
                    config
                        .base_system
                        .as_deref()
                        .unwrap_or(crate::prompt::DEFAULT_SYSTEM_MD),
                    tools.workspace(),
                    &[],
                    &bare,
                    config.environment.as_deref(),
                    config.user_preferences.as_deref(),
                    &mcp,
                    config.vision.as_deref(),
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
    /// [`CodingAgent::load_session_from`]); the next turn continues the
    /// prior history.
    ///
    /// The conversation's environment report is conversation state: a
    /// session carrying one reuses its bytes verbatim, whatever the
    /// config would render fresh — the report's date names the
    /// conversation's start, and a resume that re-stamped it would
    /// rewrite the system prompt and break the provider's prompt cache
    /// for the whole prefix. A session without one (a conversation from
    /// before the report existed) stamps the config's render and
    /// carries it forward.
    pub fn from_session(
        provider: Arc<dyn rhema_core::ModelProvider>,
        mut config: CodingAgentConfig,
        session: Session,
    ) -> Result<Self, CodingAgentError> {
        if let Some(stamped) = session
            .driver_state
            .get(ENVIRONMENT_STATE_KEY)
            .and_then(|value| value.as_str())
            && config.environment.is_some()
        {
            config.environment = Some(stamped.to_owned());
        }
        let mut agent = Self::new(provider, config)?;
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
                // extended — the stamp is skipped, not forced.
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
    /// persists the session sets this so a crash mid-turn — inside a
    /// tool call (a committed intent, which the next turn's revive
    /// closes) or mid-stream, first stream included — leaves a resumable
    /// session at the last boundary rather than no record at all.
    pub fn set_commit_hook(&mut self, hook: Option<CommitHook>) {
        self.agent.set_commit_hook(hook);
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

    /// The transcript's compaction benchmark snapshot — request
    /// counts, flushes, block size, and the providers' own prompt-cache
    /// hit rate — when the transcript is on. `None` with it off.
    pub fn transcript_stats(&self) -> Option<rhema_transcript::TranscriptStats> {
        self.transcript
            .as_ref()
            .map(|mirror| mirror.lock().expect("transcript lock poisoned").stats())
    }

    /// Folds a saved session's benchmark counters into the transcript
    /// mirror, when the transcript is on: a resumed session's stats
    /// keep reading as one run, pre-resume requests included. No-op
    /// with the transcript off — there is nothing to continue.
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
    /// requesting tools (capped at `max_turn_steps`), with `approval`
    /// gating the side-effecting tools. Appends to the session in place;
    /// the hook is cleared on the way out, success or failure. A history
    /// entering with unanswered calls — a crash resume — revives first, so
    /// the model sees a closed round, not a dangling one.
    pub async fn turn(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turn_with_cancel(input, sink, approval, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// [`Self::turn`] with a driver-held cancel flag: flipping
    /// `cancelled` mid-turn asks in-flight tools to stop (the executor
    /// polls it) and the next resolution to refuse. A driver whose stop
    /// arrives from outside the turn — a frontend's stop message, a
    /// disconnected socket — holds one flag per turn and passes it here;
    /// the plain [`Self::turn`] builds a flag nobody else can reach.
    pub async fn turn_with_cancel(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turn_message_with_cancel(Message::text(Role::User, input), sink, approval, cancelled)
            .await
    }

    /// [`Self::turn`] over a whole [`Message`]: the entry point for
    /// inputs carrying image blocks beside the text. Steering stays
    /// text — a steered nudge is a sentence, not a screenshot.
    pub async fn turn_message(
        &mut self,
        message: Message,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turn_message_with_cancel(message, sink, approval, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// [`Self::turn_message`] with [`Self::turn_with_cancel`]'s cancel
    /// flag — the full form both convenience wrappers fold into.
    pub async fn turn_message_with_cancel(
        &mut self,
        message: Message,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled,
        };
        // The hook goes on before revive: a replay-safe call re-runs
        // here, and it must do so under this turn's approval, not with
        // no hook answering "rejected" for a prompt nobody saw.
        self.tools.install_approval(approval);
        revive_dangling_calls(&mut self.session, self.resolver.as_ref(), &ctx).await;
        // With a steering source the loop is this driver's own — the
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

    /// The failed request again: no new input message — the step loop
    /// runs on the history exactly as the failed turn left it, so the
    /// same request goes out. A first-request failure re-issues the
    /// identical request; a mid-turn failure re-issues the step that
    /// failed, with the committed steps still in view (the model sees
    /// what already ran and continues from there — no committed tool
    /// call re-executes behind the model's back). A history entering
    /// with unanswered calls — a commit-hook failure after an intent
    /// landed — revives first, like any turn. Approval and cancel wire
    /// in as in [`Self::turn_message_with_cancel`].
    pub async fn retry_turn_with_cancel(
        &mut self,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled,
        };
        // The hook goes on before revive: a replay-safe call re-runs
        // here, and it must do so under this turn's approval, not with
        // no hook answering "rejected" for a prompt nobody saw.
        self.tools.install_approval(approval);
        revive_dangling_calls(&mut self.session, self.resolver.as_ref(), &ctx).await;
        let steering = self.config.steering.clone();
        let result = self.steps(None, sink, &ctx, steering.as_deref()).await;
        self.tools.clear_approval();
        Ok(result?)
    }

    /// The driver's own step loop: [`run_turn`]'s shape, generalized
    /// over the two axes the turn shapes differ on — whether a new
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
        for _ in 0..self.config.max_turn_steps {
            let step = self.agent.step(&mut self.session, sink, ctx).await?;
            if step.output.stop_reason != StopReason::ToolUse {
                return Ok(step.output);
            }
            if let Some(steering) = steering {
                self.append_notifications(steering, &mut announced);
            }
        }
        Err(AgentError::MaxIterations(self.config.max_turn_steps))
    }

    /// Drains the steering source and appends what is new as one
    /// User-role message, one line per notification. The drain is a pure
    /// query, so the same line can come back on the next drain — within
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

    /// Writes the session as pretty JSON to `path`: `0600`, landed
    /// through a temp file and renamed, so nothing conversation-shaped
    /// is ever world-readable and a crash mid-save leaves the previous
    /// checkpoint whole. This is the one store in the tree that holds
    /// the full history; it rides the same privacy helpers as every
    /// other (see `rhema_config::privacy`).
    pub fn save_session_to(&self, path: &impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.session)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        rhema_config::privacy::private_write_atomic(path.as_ref(), json.as_bytes())
    }

    /// Reads a session written by [`CodingAgent::save_session_to`].
    pub fn load_session_from(path: &impl AsRef<Path>) -> io::Result<Session> {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use rhema_agent_tools::AllowAll;
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
    async fn the_provider_sees_pastes_expanded_and_the_session_keeps_the_chips() {
        let payload = "l1\nl2\nl3\nl4";
        let embedded = rhema_core::paste::embed(
            &format!("look at {} this", rhema_core::paste::MARK),
            &[payload.to_owned()],
        );
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let mut agent = CodingAgent::new(
            recording.clone(),
            CodingAgentConfig::new("m", std::env::temp_dir()),
        )
        .unwrap();
        agent
            .turn(&embedded, &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

        // The session records the compact chip form.
        assert_eq!(
            agent.session().history[0],
            Message::text(Role::User, embedded.clone())
        );
        // The provider saw the plain expansion, never the encoding.
        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], format!("look at {payload} this"));
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

    fn blocks_agent(recording: &Arc<RecordingBlocks>) -> CodingAgent {
        CodingAgent::new(
            recording.clone(),
            CodingAgentConfig::new("m", std::env::temp_dir()),
        )
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
        agent
            .turn_message(message, &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

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
        // The session history is the message, verbatim — checkpointing
        // must carry the images byte-for-byte.
        assert_eq!(agent.session().history[0].content, seen[0]);
    }

    #[tokio::test]
    async fn the_text_entry_still_sends_exactly_one_text_block() {
        let recording = Arc::new(RecordingBlocks(Mutex::new(Vec::new())));
        let mut agent = blocks_agent(&recording);
        agent
            .turn("plain words", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], vec![Content::Text("plain words".into())]);
    }
}

#[cfg(test)]
mod extra_tools_tests {
    use super::*;
    use rhema_agent_tools::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, ToolCall,
        ToolResult, Usage, async_trait,
    };
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
        let config = CodingAgentConfig {
            extra_resolver: Some(Arc::new(Echo)),
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        };
        let mut agent = CodingAgent::new(scripted.clone(), config).unwrap();
        let output = agent
            .turn("use the tool", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

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
        let mut agent = CodingAgent::new(
            scripted.clone(),
            CodingAgentConfig::new("m", std::env::temp_dir()),
        )
        .unwrap();
        agent
            .turn("hi", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
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
        let config = CodingAgentConfig {
            transcript: None,
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        };
        let mut agent = CodingAgent::new(scripted.clone(), config).unwrap();
        agent
            .turn("hi", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
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
        let prompt = stamped_system_with_sections(
            None,
            &workspace,
            &[],
            &[],
            Some("## Environment\n\n- OS: test os"),
            Some("prefer concise answers"),
            &[],
            Some("pipe bytes into `see`"),
        )
        .unwrap();
        assert!(prompt.contains("- OS: test os"), "{prompt}");
        assert!(prompt.contains("prefer concise answers"), "{prompt}");
        assert!(prompt.contains("pipe bytes into `see`"), "{prompt}");
        // The section-less constructors render neither: an absent
        // section is the render a template without the blocks makes.
        let bare = stamped_system_with_skills(None, &workspace, &[], &[]).unwrap();
        assert!(!bare.contains("- OS: test os"), "{bare}");
        assert!(!bare.contains("prefer concise answers"), "{bare}");
        assert!(!bare.contains("pipe bytes into `see`"), "{bare}");
    }
}

#[cfg(test)]
mod environment_stamp_tests {
    use super::*;
    use rhema_agent_tools::AllowAll;
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

    fn config(report: &str) -> CodingAgentConfig {
        CodingAgentConfig {
            environment: Some(report.to_owned()),
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        }
    }

    /// A fresh conversation stamps its report at construction, before
    /// any turn: the first checkpoint already carries it.
    #[test]
    fn a_fresh_conversation_stamps_at_construction() {
        let agent = CodingAgent::new(Arc::new(Ender), config("REPORT-A")).unwrap();
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
    /// bytes, whatever the config would render fresh — the date inside
    /// names the conversation's start, and a re-stamp would rewrite
    /// the system prompt and break the prefix cache.
    #[tokio::test]
    async fn a_resumed_conversation_keeps_its_stamped_report() {
        let mut first = CodingAgent::new(Arc::new(Ender), config("REPORT-A")).unwrap();
        first
            .turn("hi", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
        let session = first.session().clone();

        // Whatever the rebuilt config carries — a model switch, a
        // profile switch, a resume — the conversation's bytes stand.
        let second =
            CodingAgent::from_session(Arc::new(Ender), config("REPORT-B"), session).unwrap();
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
        let mut first = CodingAgent::new(Arc::new(Ender), config("REPORT-A")).unwrap();
        first
            .turn("hi", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
        // Age the session: the stamp disappears, the history stays.
        let session = Session {
            driver_state: serde_json::Value::Null,
            ..first.session().clone()
        };

        let second =
            CodingAgent::from_session(Arc::new(Ender), config("REPORT-B"), session).unwrap();
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

    /// Another driver's keys coexist with the stamp — driver_state is
    /// one object per driver key — and a non-object shape is never
    /// clobbered: no stamp, no error, the section still renders.
    #[test]
    fn driver_state_shapes_coexist_or_stay_untouched() {
        let session = Session {
            driver_state: serde_json::json!({ "someone-elses": "shape" }),
            ..Session::default()
        };
        let agent =
            CodingAgent::from_session(Arc::new(Ender), config("REPORT-A"), session).unwrap();
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
        let agent = CodingAgent::from_session(Arc::new(Ender), config("REPORT-A"), opaque).unwrap();
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
    use rhema_agent_tools::AllowAll;
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
        let config = CodingAgentConfig {
            transcript: None,
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        };
        let mut agent =
            CodingAgent::from_session(recording.clone(), config, big_history(5)).unwrap();
        agent
            .turn("next", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
        let sent = recording.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // Restored history plus this turn's input, nothing rewritten.
        assert_eq!(sent[0].len(), 5 * 2 + 2);
        assert_eq!(sent[0][0], Message::text(Role::User, "fix the build"));
    }

    #[tokio::test]
    async fn on_by_default_a_big_history_is_viewed_and_the_session_stays_whole() {
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let config = CodingAgentConfig {
            transcript: Some(rhema_transcript::TranscriptSettings {
                max_view_bytes: 4_000,
                ..rhema_transcript::TranscriptSettings::default()
            }),
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        };
        let mut agent =
            CodingAgent::from_session(recording.clone(), config, big_history(5)).unwrap();
        // The prompt advertises zoom: the tool and the prompt cannot drift.
        let prompt = agent
            .system()
            .expect("the agent was built with a system prompt");
        assert!(prompt.contains("zoom"), "{prompt}");
        agent
            .turn("next", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

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

        // The session kept every message — restored, this turn's
        // input, and the turn's reply — the log is untouched.
        assert_eq!(agent.session().history.len(), 5 * 2 + 3);
    }
}
