//! The `subagent` tool: one resolver, one tool, two shapes.
//!
//! `subagent(name, prompt)` **starts** a job: [`SubagentTools`] mints a
//! job id in its [`Registry`], spawns the inner turn as a tokio task,
//! and returns in-band at once — the result names the id and the
//! notification that will follow. `subagent(id)` **fetches**: a
//! finished job's cached report (a double-fetch returns the cache,
//! never a re-run), a failed job's reason as an `is_error`, a running
//! job's "still running", an unknown id's "gone". Both shapes share one
//! honest [`ReplaySafety::Unsafe`](rhema_core::ReplaySafety) — a fetch
//! must not launder the start's classification.
//!
//! The spawned task runs the same inner path a synchronous call always
//! ran: a fresh [`Agent`](rhema_core::Agent) over the stamped parent
//! prompt with the named [`SubagentDef`]'s body appended, a bounded
//! step loop on a
//! fresh [`Session`](rhema_core::Session) whose first message is the
//! `prompt`, reading its history through the bounded transcript view.
//! The inner turn inherits five things and nothing else: the provider,
//! the approval hook (captured at spawn off the parent's
//! [`ShellTools`], held for the job's lifetime — a gate firing after
//! the parent turn's end still consults the turn's policy), the job's
//! own cancel flag (escape stops the main session, never the job), the
//! thinking effort, and the stamped system prompt every inner agent
//! runs under — the parent's own prompt (base template, skills,
//! environment, user preferences) with the definition's body appended
//! verbatim. Its inner lifecycle streams into the registry
//! as the job's status — the UI's pinned line — and never into the
//! call's result.
//!
//! A background job's own `subagent` tool keeps the synchronous shape:
//! the notification path has exactly one steering consumer — the
//! parent driver's turn loop — and a nested job would have none. A
//! nested call runs to its report inside the job's step loop.

use std::path::Path;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use serde_json::json;

use std::time::Duration;

use rhema_agent_tools::{Approval, Overlay, ShellTools};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, ModelProvider, Role, Session, ToolCall,
    ToolContext, ToolProgress, ToolResolver, ToolResult, ToolSpec,
};
use rhema_provider::Retrying;

use crate::def::SubagentDef;
use crate::registry::{Fetched, JobFailure, JobRerun, Registry, RestartJob};

/// The report is cut at this size — in bytes and in char boundaries,
/// whichever is hit first — keeping the tail: the conclusion of a report
/// sits at its end, as an error sits at the end of a command's output.
const REPORT_LIMIT: usize = 50 * 1024;

/// The waits between a background job's retries of a dead link. The
/// frontend's own ladder (~6½ minutes) rides underneath, so each of
/// these attempts re-walks it: an outage that outlasts the turn
/// ladder — minutes to hours — waits out under the job instead of
/// killing it, because a background job's caller is not sitting
/// behind the wait, while a dead job loses every step it finished.
/// The ladder runs 46 minutes of waits all told; only an outage past
/// the hour fails the job, and a transient failure is what the user's
/// retry command restarts.
const JOB_NETWORK_LADDER: &[Duration] = &[
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(30 * 60),
];

/// The waits between a background job's retries of a struggling
/// server: server failures either clear in seconds (the frontend's
/// short ladder rides underneath) or name an incident minutes cannot
/// clear, so this ladder spends three attempts, two and a half
/// minutes all told.
const JOB_SERVER_LADDER: &[Duration] = &[Duration::from_secs(30), Duration::from_secs(120)];

/// The machinery one inner run needs, shared by the background start
/// path and the synchronous nested path.
#[derive(Clone)]
struct Inner {
    /// The provider the inner agents run on: the parent's, wrapped
    /// once per run in a background-tuned [`Retrying`] — the frontend
    /// already rides its own turn ladder underneath, and the two
    /// compose into the job's tolerance (see [`JOB_NETWORK_LADDER`]).
    provider: Arc<dyn ModelProvider>,
    /// The dead-link waits a run's retry wrapper walks; the default
    /// is [`JOB_NETWORK_LADDER`], a test knob shortens it.
    network_ladder: Vec<Duration>,
    /// The struggling-server waits a run's retry wrapper walks; the
    /// default is [`JOB_SERVER_LADDER`], a test knob shortens it.
    server_ladder: Vec<Duration>,
    /// The discovered definitions; the tool's catalog.
    defs: Vec<SubagentDef>,
    /// The model identifier a definition without its own `model` runs
    /// on: the parent's.
    default_model: String,
    /// The thinking effort the inner requests carry. The default,
    /// [`ThinkingEffort::Default`], lets the provider decide — and on a
    /// thinking-by-default backend with a bounded reply budget, that
    /// decision can spend the whole budget on reasoning and leave the
    /// report empty. The parent's posture is the safe inheritance.
    thinking: ThinkingEffort,
    /// How many nesting levels remain. One is the default: a subagent
    /// gets no `subagent` tool of its own. Zero advertises nothing.
    max_depth: u32,
    /// The stamped system prompt every inner agent runs under: the
    /// parent's full prompt — base template, skills, environment
    /// facts, user preferences, the subagent's own tool list —
    /// computed once at wire time off the config. Empty on direct
    /// constructions (tests): the body runs alone.
    base: String,
}

impl Inner {
    /// The inner shell for one run: fresh, with `hook` — captured off
    /// the parent shell at spawn — installed. `None` means the inner
    /// tools reject: the fail-safe default, never the case mid-turn.
    fn inner_shell(
        &self,
        parent: &Arc<ShellTools>,
        hook: Option<Arc<dyn Approval>>,
    ) -> Arc<ShellTools> {
        let shell = Arc::new(
            ShellTools::with_executor(parent.workspace().to_path_buf(), parent.executor())
                // The executor carries the wiring's env, so `see` exists in
                // the inner commands; the inbox inherits so their staged
                // images attach to the inner results.
                .with_vision_inbox(parent.vision_inbox().map(Path::to_path_buf)),
        );
        if let Some(hook) = hook {
            shell.install_approval(hook);
        }
        shell
    }

    /// The inner resolver over the fresh shell: the shell alone, plus a
    /// nested synchronous `SubagentTools` when the nesting budget
    /// allows one.
    fn inner_resolver(&self, inner_shell: &Arc<ShellTools>) -> Arc<dyn ToolResolver> {
        if self.max_depth <= 1 {
            return inner_shell.clone();
        }
        let nested = SubagentTools::synchronous(
            self.clone_with_depth(self.max_depth - 1),
            inner_shell.clone(),
        );
        Arc::new(Overlay::new(inner_shell.clone(), Arc::new(nested)))
    }

    fn clone_with_depth(&self, max_depth: u32) -> Self {
        Self {
            max_depth,
            ..self.clone()
        }
    }

    /// Runs one definition to its report, or to the reason it failed.
    async fn run(
        &self,
        def: &SubagentDef,
        prompt: &str,
        ctx: &ToolContext,
        inner_shell: &Arc<ShellTools>,
        progress: &mut dyn ToolProgress,
    ) -> Result<String, JobFailure> {
        // The inner system prompt: the stamp with this definition's
        // body appended verbatim. A body is plain text, never
        // rendered — template syntax in it reaches the model as the
        // author wrote it, so a definition written for another
        // harness behaves as written. An empty stamp (direct
        // constructions, tests) runs the body alone.
        let system = if self.base.is_empty() {
            def.body.clone()
        } else {
            format!("{}\n\n{}", self.base, def.body)
        };
        let resolver = self.inner_resolver(inner_shell);
        // Each run wraps the provider in its own fresh retry and
        // transcript: the wrapper holds no state across requests, so a
        // per-run instance is the shared one's exact equal, and the
        // inner history lives and dies with the run — no mirror to
        // share, no per-definition settings to plumb. The `zoom` half
        // of `attach` is dropped on purpose — the subagent's tool
        // surface stays the shell alone.
        let retrying = Arc::new(Retrying::with_ladders(
            self.provider.clone(),
            self.network_ladder.clone(),
            self.server_ladder.clone(),
        ));
        let (provider, _zoom) =
            rhema_transcript::attach(retrying, rhema_transcript::TranscriptSettings::default());
        let mut agent = Agent::builder()
            .provider(provider)
            .resolver(resolver)
            .system(system)
            .params(ChatParams {
                thinking: self.thinking,
                ..ChatParams::new(
                    def.model
                        .clone()
                        .unwrap_or_else(|| self.default_model.clone()),
                )
            })
            .build()
            .map_err(|err| JobFailure::Permanent(format!("its engine failed to build: {err}")))?;

        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        let inner_ctx = ToolContext {
            cwd: inner_shell.workspace().to_path_buf(),
            run_id: format!("{}/subagent:{}", ctx.run_id, def.name),
            cancelled: ctx.cancelled.clone(),
        };
        let model = def
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        let mut sink = Forwarder {
            progress,
            model: model.clone(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };

        for _ in 0..def.max_steps() {
            if ctx.cancelled.load(Ordering::Relaxed) {
                return Err(JobFailure::Permanent("cancelled".to_owned()));
            }
            let step = agent
                .step(&mut session, &mut sink, &inner_ctx)
                .await
                .map_err(|err| {
                    // The failure class is decided where the error is
                    // known: a retryable provider failure names the
                    // infrastructure, everything else names the work.
                    if err.is_retryable() {
                        JobFailure::Transient(err.to_string())
                    } else {
                        JobFailure::Permanent(err.to_string())
                    }
                })?;
            if step.output.stop_reason != rhema_core::StopReason::ToolUse {
                let report: String = step
                    .output
                    .message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if report.trim().is_empty() {
                    return Err(JobFailure::Permanent(
                        "finished without a report".to_owned(),
                    ));
                }
                return Ok(capped(report));
            }
        }
        Err(JobFailure::Permanent(format!(
            "hit its step budget of {}",
            def.max_steps()
        )))
    }
}

/// Runs subagents behind the `subagent` tool.
pub struct SubagentTools {
    inner: Inner,
    /// The parent's shell. Two jobs: the workspace the subagent's tools
    /// run in, and the current turn's approval hook the inner shell
    /// inherits at spawn.
    shell: Arc<ShellTools>,
    /// The job registry, when this instance starts background jobs.
    /// `None` on the nested synchronous instances: their calls run to
    /// their report inside the caller's step loop and register
    /// nowhere.
    registry: Option<Arc<Registry>>,
}

impl SubagentTools {
    /// A resolver over `defs`, running on the parent's `provider` and
    /// `shell`, defaulting to one level of nesting (none) and a
    /// registry capped at [`crate::registry::DEFAULT_MAX_RUNNING`]
    /// concurrent jobs. Calls start background jobs and return job ids.
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        defs: Vec<SubagentDef>,
        shell: Arc<ShellTools>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            inner: Inner {
                provider,
                network_ladder: JOB_NETWORK_LADDER.to_vec(),
                server_ladder: JOB_SERVER_LADDER.to_vec(),
                defs,
                default_model: default_model.into(),
                thinking: ThinkingEffort::Default,
                max_depth: 1,
                base: String::new(),
            },
            shell,
            registry: Some(Arc::new(Registry::new(
                crate::registry::DEFAULT_MAX_RUNNING,
            ))),
        }
    }

    /// Replaces the retry ladders a run's wrapper walks — the
    /// background-tuned defaults exist so a job waits out an outage,
    /// and this knob exists for tests that cannot sit through one.
    /// Call before the first job starts.
    pub fn with_retry_ladders(
        mut self,
        network_ladder: Vec<Duration>,
        server_ladder: Vec<Duration>,
    ) -> Self {
        self.inner.network_ladder = network_ladder;
        self.inner.server_ladder = server_ladder;
        self
    }

    /// The synchronous variant: a call runs the subagent to its report
    /// inside the resolving step, exactly as the tool behaved before
    /// the registry. Reserved for the nested level inside a background
    /// job — the notification path has one steering consumer, and a
    /// nested job would have none.
    fn synchronous(inner: Inner, shell: Arc<ShellTools>) -> Self {
        Self {
            inner,
            shell,
            registry: None,
        }
    }

    /// Sets the thinking effort the inner requests carry; the caller
    /// passes the parent turn's, so a subagent samples the way the
    /// agent that delegated it samples.
    pub fn with_thinking(mut self, thinking: ThinkingEffort) -> Self {
        self.inner.thinking = thinking;
        self
    }

    /// Sets the stamped system prompt the inner agents run under —
    /// the parent's, computed once at wire time; the definition's
    /// body lands under it verbatim. Call before the first job.
    pub fn with_base_system(mut self, base: String) -> Self {
        self.inner.base = base;
        self
    }

    /// Sets the remaining nesting budget; each nested level gets one
    /// less, and a subagent at zero is advertised no `subagent` tool.
    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.inner.max_depth = max_depth;
        self
    }

    /// Caps the concurrent top-level jobs; starts past the cap queue
    /// (id minted and returned at once) until a slot frees. Call before
    /// the first job starts.
    pub fn with_max_jobs(mut self, max_running: usize) -> Self {
        self.registry = Some(Arc::new(Registry::new(max_running)));
        self
    }

    /// The status handle backed by this tool's registry: the UI's
    /// subscription, and — through [`Registry`]'s [`Steering`]
    /// implementation — the driver's notification source. The same
    /// instance the tool resolver starts jobs into, held weakly so the
    /// handle never keeps the tool alive; a restart past the drop is
    /// an empty answer.
    pub fn handle(self: &Arc<Self>) -> crate::wire::SubagentHandle {
        crate::wire::SubagentHandle::from_tools(self)
    }

    /// The registry, when this instance starts background jobs. A
    /// synchronous (nested) instance registers nowhere and answers
    /// with a one-slot stand-in, the shape [`Self::handle`] would have
    /// built.
    pub(crate) fn registry(&self) -> Arc<Registry> {
        self.registry
            .clone()
            .unwrap_or_else(|| Arc::new(Registry::new(1)))
    }

    /// The `subagent` tool spec over `defs`: the catalog and the
    /// protocol ride the description, so the prompt's tool list teaches
    /// the model when to delegate and how to fetch with no renderer
    /// change.
    pub fn spec(defs: &[SubagentDef]) -> ToolSpec {
        Self::spec_for(defs, true)
    }

    /// [`Self::spec`] per shape: the background spec carries the
    /// start/notification/fetch protocol; the synchronous (nested) one
    /// the report-as-result contract.
    fn spec_for(defs: &[SubagentDef], background: bool) -> ToolSpec {
        let mut description = if background {
            String::from(
                "Run a named subagent in the background: an independent \
                 agent with its own conversation, system prompt, and \
                 workspace tools, which works the task and produces a \
                 final report. The subagent does not see this \
                 conversation and its report is all you get back, so the \
                 prompt must carry everything it needs.\n\
                 \n\
                 How it works: call with `name` and `prompt` to start a \
                 subagent — the call returns a job id at once and the \
                 subagent keeps working in the background. When a job \
                 finishes you will receive a notification naming it; \
                 fetch its report by calling this tool with only `id` \
                 set to that job id (for example \"job-3\"). A fetch of \
                 a job that is still running is an error: wait for the \
                 notification. A finished job's report is cached — \
                 fetching again returns the same report, never a re-run. \
                 Jobs keep running across turns, and stopping this \
                 session never stops one.",
            )
        } else {
            String::from(
                "Run a named subagent: an independent agent with its own \
                 conversation, system prompt, and workspace tools, which \
                 works the task and returns its final report as the tool \
                 result. The subagent does not see this conversation and \
                 its report is all you get back, so the prompt must \
                 carry everything it needs.",
            )
        };
        for def in defs {
            description.push_str(&format!("\n- {}: {}", def.name, def.description));
        }
        if background {
            ToolSpec::new(
                "subagent",
                description,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The subagent to start, from the catalog in this tool's description."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The full task for the subagent: what to do, and what to report back."
                        },
                        "id": {
                            "type": "string",
                            "description": "A job id to fetch the report of, instead of starting a new subagent."
                        }
                    }
                }),
            )
        } else {
            ToolSpec::new(
                "subagent",
                description,
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The subagent to run, from the catalog in this tool's description."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The full task for the subagent: what to do, and what to report back."
                        }
                    },
                    "required": ["name", "prompt"]
                }),
            )
        }
    }

    /// Validates the call's arguments into the start it names or the
    /// fetch it asks for, or the failure result to return instead.
    fn parse_call<'a>(&'a self, call: &'a ToolCall) -> Result<Parsed<'a>, ToolResult> {
        let invalid = |why: String| ToolResult::failure(&call.id, why);
        let id = call.args.get("id").and_then(|value| value.as_str());
        let name = call.args.get("name").and_then(|value| value.as_str());
        let prompt = call.args.get("prompt").and_then(|value| value.as_str());
        match (id, name, prompt) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(invalid(
                "pass `name` and `prompt` to start a subagent, or `id` to \
                 fetch a started one — not both"
                    .to_owned(),
            )),
            (Some(id), None, None) => {
                if id.trim().is_empty() {
                    return Err(invalid("the `id` argument is empty".to_owned()));
                }
                Ok(Parsed::Fetch { id })
            }
            (None, _, _) => {
                let name = name.ok_or_else(|| {
                    invalid(
                        "the subagent tool needs a string `name` to start a \
                         subagent, or a string `id` to fetch a started one"
                            .to_owned(),
                    )
                })?;
                let prompt = prompt.ok_or_else(|| {
                    invalid("the subagent tool needs a string `prompt`".to_owned())
                })?;
                let def = self
                    .inner
                    .defs
                    .iter()
                    .find(|def| def.name == name)
                    .ok_or_else(|| {
                        invalid(format!(
                            "no subagent named {name}; available: {}",
                            self.inner
                                .defs
                                .iter()
                                .map(|def| def.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                if prompt.trim().is_empty() {
                    return Err(invalid("the `prompt` argument is empty".to_owned()));
                }
                Ok(Parsed::Start { def, prompt })
            }
        }
    }

    /// Starts one job: the id is minted and returned at once, the run
    /// rides a tokio task holding its own cancel flag and the approval
    /// hook captured at spawn. The spawn ingredients arm the job for a
    /// later restart.
    fn start(
        &self,
        call_id: &str,
        def: &SubagentDef,
        prompt: &str,
        ctx: &ToolContext,
    ) -> ToolResult {
        let registry = self
            .registry
            .clone()
            .expect("the background path starts into a registry");
        let minted = registry.mint(&def.name);
        // The hook is read at spawn and held for the job's lifetime: a
        // gate firing after the parent turn's end still consults the
        // turn's policy, never the fail-safe reject. No hook installed
        // means the inner tools reject — the fail-safe default.
        let hook = self.shell.current_approval();
        let rerun = JobRerun {
            def: def.clone(),
            prompt: prompt.to_owned(),
            parent_run: ctx.run_id.clone(),
            hook: hook.clone(),
        };
        registry.arm_restart(&minted.id, rerun.clone());
        let id = minted.id.clone();
        self.spawn(&registry, &minted.id, minted.cancel.clone(), rerun);
        ToolResult::success(
            call_id.to_owned(),
            format!(
                "started subagent {} as {id} in the background. When its \
                 report is announced in a notification, fetch it by \
                 calling this tool with id \"{id}\".",
                def.name
            ),
        )
    }

    /// Spawns one job's run as a tokio task: the shared body of a
    /// start and a restart. A restart passes the ingredients
    /// [`Registry::restart_failed`] handed back and the job's fresh
    /// cancel flag; the id stays the job's own.
    fn spawn(&self, registry: &Arc<Registry>, id: &str, cancel: Arc<AtomicBool>, rerun: JobRerun) {
        let inner = self.inner.clone();
        let inner_shell = self.inner.inner_shell(&self.shell, rerun.hook.clone());
        let model = rerun
            .def
            .model
            .clone()
            .unwrap_or_else(|| self.inner.default_model.clone());
        let task_id = id.to_owned();
        let task_name = rerun.def.name.clone();
        let weak_registry = Arc::downgrade(registry);
        let progress = StatusPump {
            registry: Arc::downgrade(registry),
            id: task_id.clone(),
        };
        tokio::spawn(async move {
            // Queue, not reject: past the cap the id was already
            // returned; the run begins when a slot frees. A dropped
            // registry ends the wait with None — the agent is gone.
            let Some(permit) = crate::registry::acquire_slot(&weak_registry).await else {
                return;
            };
            // Touch the registry only in short critical sections: the
            // task holds the Weak, never a strong ref across the run —
            // a dropped registry must actually drop, and its flag stops
            // the job at the next step boundary.
            if let Some(registry) = weak_registry.upgrade() {
                registry.mark_running(&task_id, Some(model.clone()));
            }
            let ctx = ToolContext {
                cwd: inner_shell.workspace().to_path_buf(),
                run_id: format!("{}/{}/subagent:{}", rerun.parent_run, task_id, task_name),
                cancelled: cancel,
            };
            let mut progress = progress;
            let outcome = inner
                .run(&rerun.def, &rerun.prompt, &ctx, &inner_shell, &mut progress)
                .await;
            if let Some(registry) = weak_registry.upgrade() {
                registry.finish(&task_id, outcome);
            }
            drop(permit);
        });
    }

    /// Restarts every transient-failed job: the registry resets the
    /// records and hands back the ingredients, and this re-spawns each
    /// run under its own id. The names come back for the frontend's
    /// notice; the re-runs announce themselves when they finish, into
    /// the same ids the model already knows.
    pub fn restart_failed_jobs(&self) -> Vec<crate::registry::Restarted> {
        let Some(registry) = &self.registry else {
            return Vec::new();
        };
        registry
            .restart_failed()
            .into_iter()
            .map(|job| {
                let RestartJob {
                    id,
                    name,
                    rerun,
                    cancel,
                } = job;
                self.spawn(registry, &id, cancel, rerun);
                crate::registry::Restarted { id, name }
            })
            .collect()
    }
}

/// The validated shape of one `subagent` call.
enum Parsed<'a> {
    /// `name` + `prompt`: start a job over this definition.
    Start {
        def: &'a SubagentDef,
        prompt: &'a str,
    },
    /// `id` alone: fetch a started job's report.
    Fetch { id: &'a str },
}

#[async_trait]
impl ToolResolver for SubagentTools {
    fn specs(&self) -> Vec<ToolSpec> {
        if self.inner.max_depth == 0 || self.inner.defs.is_empty() {
            return Vec::new();
        }
        vec![Self::spec_for(&self.inner.defs, self.registry.is_some())]
    }

    fn replay_safety(&self, name: &str) -> rhema_core::ReplaySafety {
        let _ = name;
        // A subagent can side-effect through its shell, and the fetch
        // is the start's own shape: never re-run either that lost its
        // result to a crash.
        rhema_core::ReplaySafety::Unsafe
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        self.resolve_streaming(call, ctx, &mut rhema_core::OutputOnly(|_| {}))
            .await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut dyn ToolProgress,
    ) -> ToolResult {
        let id = call.id.clone();
        match self.parse_call(&call) {
            Err(result) => return result,
            Ok(Parsed::Fetch { id: job }) => {
                let Some(registry) = &self.registry else {
                    return ToolResult::failure(
                        id,
                        "this subagent tool runs synchronously; pass `name` \
                         and `prompt` to run one"
                            .to_owned(),
                    );
                };
                match registry.fetch(job) {
                    Fetched::Report { report, .. } => ToolResult::success(id, report),
                    Fetched::Failed { name, reason } => {
                        ToolResult::failure(id, format!("subagent {name}: {reason}"))
                    }
                    Fetched::Busy { name } => ToolResult::failure(
                        id,
                        format!(
                            "subagent {name} ({job}) is still running; wait \
                             for the notification announcing its report, \
                             then fetch again"
                        ),
                    ),
                    Fetched::Unknown => ToolResult::failure(
                        id,
                        format!(
                            "no such job: {job} is gone — jobs live in this \
                             agent's memory and are lost on restart; start \
                             a new subagent"
                        ),
                    ),
                }
            }
            Ok(Parsed::Start { def, prompt }) if self.registry.is_some() => {
                self.start(&id, def, prompt, ctx)
            }
            Ok(Parsed::Start { def, prompt }) => {
                // The synchronous (nested) shape: run to the report
                // inside the resolving step, the hook captured here as
                // anywhere.
                let hook = self.shell.current_approval();
                let inner_shell = self.inner.inner_shell(&self.shell, hook);
                let outcome = self
                    .inner
                    .run(def, prompt, ctx, &inner_shell, progress)
                    .await;
                inner_shell.clear_approval();
                match outcome {
                    Ok(report) => ToolResult::success(id, report),
                    Err(why) => {
                        ToolResult::failure(id, format!("subagent {}: {}", def.name, why.reason()))
                    }
                }
            }
        }
    }
}

/// Pumps a background job's inner lifecycle into the registry: the
/// statuses the forwarder reports become the job's status, the line the
/// UI renders. The inner output rides no channel — the job is its own
/// line, and its report is the fetch.
struct StatusPump {
    registry: Weak<Registry>,
    id: String,
}

impl ToolProgress for StatusPump {
    fn output(&mut self, _chunk: &str) {}

    fn status(&mut self, status: rhema_core::ToolStatus) {
        if let Some(registry) = self.registry.upgrade() {
            registry.report(&self.id, &status);
        }
    }
}

/// Renders the inner turn's lifecycle into the call's own status: the
/// phase the subagent is in, the context its current model call
/// carries, and what its model calls have spent so far. The inner
/// turn's text and tool output go nowhere — the subagent is
/// its own line on the parent's screen, and its report is the result;
/// the lifecycle events themselves stay private, the status is
/// screen-facing, never history. A nested subagent's status is dropped:
/// to this call the nested subagent is just one running tool.
struct Forwarder<'a> {
    progress: &'a mut dyn ToolProgress,
    /// The model id the inner turn runs on, stamped into every status.
    model: String,
    /// The phase last reported; only transitions are sent.
    phase: Option<String>,
    /// The current model call's usage: the latest running tally, held
    /// past [`Event::StepEnd`](rhema_core::Event::StepEnd) until the
    /// next call's first frame replaces it — the reply rides into the
    /// next request, so input + output stays the right current-context
    /// figure between steps.
    context: Option<rhema_core::Usage>,
    /// Cumulative spend across the call's steps. Only a `StepEnd` adds
    /// into it: the running frames already cover their step's final
    /// tally, and summing them would double-count.
    spent: rhema_core::Usage,
}

/// The phase words a status line reads, in the order the inner turn
/// moves through them.
const THINKING: &str = "thinking";
const RUNNING: &str = "running";
const WRITING: &str = "writing output";

impl Forwarder<'_> {
    /// Reports a phase transition with the spend so far; a repeated
    /// phase sends nothing.
    fn report(&mut self, phase: impl Into<String>) {
        let phase = phase.into();
        if self.phase.as_deref() == Some(phase.as_str()) {
            return;
        }
        self.phase = Some(phase);
        self.send();
    }

    /// Sends the current phase, context, and spend.
    fn send(&mut self) {
        self.progress.status(rhema_core::ToolStatus {
            phase: self.phase.clone().unwrap_or_else(|| THINKING.to_owned()),
            context: self.context,
            spent: Some(self.spent),
            model: Some(self.model.clone()),
        });
    }
}

impl ToolProgress for Forwarder<'_> {
    fn output(&mut self, _chunk: &str) {}

    fn status(&mut self, _status: rhema_core::ToolStatus) {}
}

impl EventSink for Forwarder<'_> {
    fn emit(&mut self, event: Event<'_>) {
        match event {
            // The model is streaming: reasoning is thinking, text is the
            // report being written.
            Event::StepStart { .. } | Event::ThinkingDelta(_) => self.report(THINKING),
            Event::TextDelta(_) => self.report(WRITING),
            // A tool of the subagent's is executing.
            Event::ToolCallStart { .. } => self.report(RUNNING),
            Event::ToolResult { .. } => self.report(THINKING),
            Event::StepEnd { usage, .. } => {
                self.spent.input_tokens =
                    self.spent.input_tokens.saturating_add(usage.input_tokens);
                self.spent.output_tokens =
                    self.spent.output_tokens.saturating_add(usage.output_tokens);
                self.spent.cached_input_tokens = self
                    .spent
                    .cached_input_tokens
                    .saturating_add(usage.cached_input_tokens);
                self.context = Some(usage);
                self.send();
            }
            // A retry announcement means the model call died and the
            // ladder is waiting: the line names the wait, so a job
            // riding out an outage reads as waiting, not hung. One
            // report per announcement — the attempt number moves — and
            // the next delta returns the flow phases.
            Event::RetryWait { attempt, total, .. } => {
                self.report(format!("retrying model call (attempt {attempt}/{total})"));
            }
            // A nested subagent's status stays private to it, argument
            // streaming is not a phase of its own, and the inner
            // output's lines ride no channel. A running tally replaces
            // the context — it is the current call's figure, never a
            // spend to accumulate; the accounting still rides `StepEnd`.
            Event::ToolStatus { .. }
            | Event::ToolCallArgsDelta { .. }
            | Event::ToolOutputDelta { .. } => {}
            Event::Usage(usage) => {
                if self.context.as_ref() != Some(&usage) {
                    self.context = Some(usage);
                    self.send();
                }
            }
        }
    }
}

/// Caps a report to [`REPORT_LIMIT`], keeping the tail.
fn capped(report: String) -> String {
    if report.len() <= REPORT_LIMIT {
        return report;
    }
    let mut start = report.len() - REPORT_LIMIT;
    while !report.is_char_boundary(start) {
        start += 1;
    }
    format!("[...truncated...]\n{}", &report[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_oversized_report_keeps_its_tail() {
        let head = "h".repeat(200);
        let tail = "t".repeat(REPORT_LIMIT);
        let out = capped(format!("{head}{tail}"));
        assert!(out.starts_with("[...truncated...]\n"));
        assert!(out.ends_with(&tail));
        assert!(out.len() < head.len() + tail.len());
    }

    #[test]
    fn a_within_limit_report_passes_through() {
        assert_eq!(capped("short".to_owned()), "short");
    }

    #[test]
    fn the_spec_carries_the_catalog_and_the_protocol() {
        let defs = vec![
            SubagentDef {
                name: "reviewer".into(),
                description: "reviews diffs".into(),
                model: None,
                max_steps: None,
                body: "Review.".into(),
            },
            SubagentDef {
                name: "scout".into(),
                description: "explores the tree".into(),
                model: None,
                max_steps: None,
                body: "Explore.".into(),
            },
        ];
        let spec = SubagentTools::spec(&defs);
        assert_eq!(spec.name, "subagent");
        assert!(spec.description.contains("reviewer: reviews diffs"));
        assert!(spec.description.contains("scout: explores the tree"));
        // The protocol: start, notification, fetch, cache.
        assert!(
            spec.description.contains("background"),
            "{}",
            spec.description
        );
        assert!(spec.description.contains("notification"));
        assert!(spec.description.contains("job id"));
        assert!(spec.description.contains("never a re-run"));
        // The schema carries the optional id beside name and prompt.
        let properties = spec.parameters["properties"].as_object().unwrap();
        assert!(properties.contains_key("name"));
        assert!(properties.contains_key("prompt"));
        assert!(properties.contains_key("id"));
    }

    /// A progress channel that keeps everything it is handed.
    struct Collector {
        lines: Vec<String>,
        statuses: Vec<rhema_core::ToolStatus>,
    }

    impl ToolProgress for Collector {
        fn output(&mut self, chunk: &str) {
            self.lines.push(chunk.to_owned());
        }
        fn status(&mut self, status: rhema_core::ToolStatus) {
            self.statuses.push(status);
        }
    }

    #[test]
    fn the_inner_lifecycle_becomes_phase_transitions_and_a_spend() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "parent-model".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        // Reasoning first, then a tool, its output, the step's spend,
        // then the report being written.
        sink.emit(Event::ThinkingDelta("hmm"));
        sink.emit(Event::ToolCallStart {
            id: "c1",
            name: "bash",
        });
        sink.emit(Event::StepEnd {
            usage: rhema_core::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cached_input_tokens: 80,
            },
            stop_reason: &rhema_core::StopReason::ToolUse,
        });
        sink.emit(Event::TextDelta("the report"));
        drop(sink);

        // The phases arrived as transitions; the step's spend re-sent
        // the running phase with the figures: thinking → running (the
        // tool) → running again (the spend) → writing output (the
        // report).
        let phases: Vec<&str> = collected
            .statuses
            .iter()
            .map(|status| status.phase.as_str())
            .collect();
        assert_eq!(phases, vec![THINKING, RUNNING, RUNNING, WRITING]);
        let last = collected.statuses.last().unwrap();
        assert_eq!(last.model.as_deref(), Some("parent-model"));
        // The step's call is the current context...
        let context = last.context.unwrap();
        assert_eq!(context.input_tokens, 100);
        assert_eq!(context.output_tokens, 20);
        assert_eq!(context.cached_input_tokens, 80);
        // ...and the only spend so far.
        let spent = last.spent.unwrap();
        assert_eq!(spent.input_tokens, 100);
        assert_eq!(spent.output_tokens, 20);
        assert_eq!(spent.cached_input_tokens, 80);

        // The inner output rides no channel: the subagent is its own
        // line, its report the result.
        assert!(collected.lines.is_empty());
    }

    /// A retry announcement is a phase of its own: a job riding out an
    /// outage reads as waiting, with the attempt count, not hung.
    #[test]
    fn a_retry_wait_becomes_a_retrying_phase() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "m".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        sink.emit(Event::ThinkingDelta("hmm"));
        sink.emit(Event::RetryWait {
            error: "network error: dead link",
            delay: Duration::from_secs(30),
            attempt: 2,
            total: 6,
        });
        sink.emit(Event::RetryWait {
            error: "network error: dead link",
            delay: Duration::from_secs(60),
            attempt: 3,
            total: 6,
        });
        sink.emit(Event::TextDelta("still here"));
        drop(sink);

        let phases: Vec<&str> = collected
            .statuses
            .iter()
            .map(|status| status.phase.as_str())
            .collect();
        assert_eq!(
            phases,
            vec![
                THINKING,
                "retrying model call (attempt 2/6)",
                "retrying model call (attempt 3/6)",
                WRITING,
            ]
        );
    }

    #[test]
    fn repeated_phases_send_nothing() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "m".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        sink.emit(Event::ThinkingDelta("a"));
        sink.emit(Event::ThinkingDelta("b"));
        assert_eq!(collected.statuses.len(), 1);
    }

    /// A running tally is the current call's context, never a spend: a
    /// mid-call frame replaces the context without touching the spend,
    /// and only the step's end bills into the spend. The context holds
    /// past a `StepEnd` — the reply rides into the next request — until
    /// the next call's first frame replaces it.
    #[test]
    fn a_running_tally_is_context_and_only_a_step_end_is_spend() {
        let mut collected = Collector {
            lines: Vec::new(),
            statuses: Vec::new(),
        };
        let mut sink = Forwarder {
            progress: &mut collected,
            model: "m".into(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        sink.emit(Event::ThinkingDelta("hmm"));
        sink.emit(Event::Usage(rhema_core::Usage {
            input_tokens: 40_000,
            output_tokens: 10,
            cached_input_tokens: 0,
        }));
        sink.emit(Event::StepEnd {
            usage: rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            },
            stop_reason: &rhema_core::StopReason::ToolUse,
        });
        sink.emit(Event::ToolCallStart {
            id: "c2",
            name: "bash",
        });
        sink.emit(Event::Usage(rhema_core::Usage {
            input_tokens: 41_000,
            output_tokens: 20,
            cached_input_tokens: 40_000,
        }));
        drop(sink);

        // The thinking status carried the running frame as its context
        // and nothing billed yet.
        let thinking = &collected.statuses[1];
        assert_eq!(thinking.phase, THINKING);
        assert_eq!(
            thinking.context,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 10,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(
            thinking.spent,
            Some(rhema_core::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
            })
        );

        // The step's end billed the spend and held the context; the
        // running status kept both...
        let running = &collected.statuses[3];
        assert_eq!(running.phase, RUNNING);
        assert_eq!(
            running.context,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );
        assert_eq!(
            running.spent,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );

        // ...and the next call's frame replaced the context, the spend
        // still counting every step.
        let next = collected.statuses.last().unwrap();
        assert_eq!(
            next.context,
            Some(rhema_core::Usage {
                input_tokens: 41_000,
                output_tokens: 20,
                cached_input_tokens: 40_000,
            })
        );
        assert_eq!(
            next.spent,
            Some(rhema_core::Usage {
                input_tokens: 40_000,
                output_tokens: 500,
                cached_input_tokens: 0,
            })
        );
    }

    /// A background job's statuses land in the registry through the
    /// status pump: the job's record moves with its inner turn.
    #[test]
    fn the_status_pump_reports_into_the_registry() {
        let registry = Arc::new(Registry::new(1));
        let minted = registry.mint("scout");
        registry.mark_running(&minted.id, Some("m".into()));
        let mut pump = StatusPump {
            registry: Arc::downgrade(&registry),
            id: minted.id.clone(),
        };
        pump.status(rhema_core::ToolStatus {
            phase: RUNNING.to_owned(),
            context: Some(rhema_core::Usage::default()),
            spent: Some(rhema_core::Usage::default()),
            model: Some("m".into()),
        });
        assert_eq!(registry.status()[0].phase, RUNNING);
    }
}
