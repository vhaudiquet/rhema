//! The `subagent` tool: one resolver, one tool, two shapes.
//!
//! `subagent(name, prompt)` **starts** a job: [`SubagentTools`] mints a
//! job id in its [`Registry`], spawns the inner turn as a tokio task,
//! and returns in-band at once, the result names the id and the
//! notification that will follow. `subagent(id)` **fetches**: a
//! finished job's cached report (a double-fetch returns the cache,
//! never a re-run), a failed job's reason as an `is_error`, a running
//! job's "still running", an unknown id's "gone". Both shapes share one
//! honest [`ReplaySafety::Unsafe`](rhema_core::ReplaySafety), a fetch
//! must not launder the start's classification.
//!
//! The spawned task runs the same inner path a synchronous call always
//! ran: a fresh [`Agent`] over the stamped parent
//! prompt with the named [`SubagentDef`]'s body appended, a bounded
//! step loop over the run's session, reading its history through the
//! bounded transcript view. A first run's session is fresh, its first
//! message the `prompt`; a restart continues the child session the
//! failed run already committed, its spent history intact, so nothing
//! the old run earned is thrown away or re-run.
//! The inner turn inherits five things and nothing else: where it
//! rides, the call's optional `model` argument over the definition's
//! frontmatter over the parent's provider and model, the first two
//! free to move to another provider the frontend is logged into, the
//! approval hook (captured at spawn off the parent's
//! [`ShellTools`], held for the job's lifetime, a gate firing after
//! the parent turn's end still consults the turn's policy), the job's
//! own cancel flag (escape stops the main session, never the job), the
//! thinking effort, and the stamped system prompt every inner agent
//! runs under, the parent's own prompt (base template, skills,
//! environment, user preferences) with the definition's body appended
//! verbatim. Its inner lifecycle streams into the registry
//! as the job's status, the UI's pinned line, and never into the
//! call's result.
//!
//! A background job's own `subagent` tool keeps the synchronous shape:
//! the notification path has exactly one steering consumer, the
//! parent driver's turn loop, and a nested job would have none. A
//! nested call runs to its report inside the job's step loop.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::json;

use std::time::Duration;

use rhema_bash::{Approval, ShellTools};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, ModelProvider, Overlay, Role, Session,
    ToolCall, ToolContext, ToolProgress, ToolResolver, ToolResult, ToolSpec,
};
use rhema_host_agent::SubagentProviders;
use rhema_provider::Retrying;
use rhema_session_store::{SessionId, SessionStore};

use crate::def::SubagentDef;
use crate::kind;
use crate::registry::{Fetched, JobFailure, JobRerun, Registry, RestartJob};

/// The report is cut at this size, in bytes and in char boundaries,
/// whichever is hit first, keeping the tail: the conclusion of a report
/// sits at its end, as an error sits at the end of a command's output.
const REPORT_LIMIT: usize = 50 * 1024;

/// The waits between a background job's retries of a dead link. The
/// frontend's own ladder (~6½ minutes) rides underneath, so each of
/// these attempts re-walks it: an outage that outlasts the turn
/// ladder, minutes to hours, waits out under the job instead of
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
    /// The provider the inner agents run on by default: the parent's,
    /// wrapped once per run in a background-tuned [`Retrying`], the
    /// frontend already rides its own turn ladder underneath, and the
    /// two compose into the job's tolerance (see
    /// [`JOB_NETWORK_LADDER`]). A `model` argument naming another
    /// provider resolves through [`Inner::providers`] instead.
    provider: Arc<dyn ModelProvider>,
    /// Resolves a provider kind to a turn provider: the frontend's
    /// credential policy behind the `<provider>/<model>` spelling in
    /// the call's `model` argument and a definition's frontmatter.
    /// `None` means the session's provider is the only one, a
    /// cross-provider spelling fails with that named.
    providers: Option<SubagentProviders>,
    /// The kinds [`Inner::providers`] resolved when it was installed,
    /// probed once at wire time: the provider list the spec teaches.
    /// Credential answers can change mid-session (a `/login`), so a
    /// call resolves through the factory live and this list only
    /// informs the prompt.
    provider_kinds: Vec<&'static str>,
    /// The dead-link waits a run's retry wrapper walks; the default
    /// is [`JOB_NETWORK_LADDER`], a test knob shortens it.
    network_ladder: Vec<Duration>,
    /// The struggling-server waits a run's retry wrapper walks; the
    /// default is [`JOB_SERVER_LADDER`], a test knob shortens it.
    server_ladder: Vec<Duration>,
    /// The discovered definitions; the tool's catalog.
    defs: Vec<SubagentDef>,
    /// The model identifier a call and a definition without their own
    /// `model` run on: the parent's.
    default_model: String,
    /// The thinking effort the inner requests carry. The default,
    /// [`ThinkingEffort::Default`], lets the provider decide, and on a
    /// thinking-by-default backend with a bounded reply budget, that
    /// decision can spend the whole budget on reasoning and leave the
    /// report empty. The parent's posture is the safe inheritance.
    thinking: ThinkingEffort,
    /// How many nesting levels remain. One is the default: a subagent
    /// gets no `subagent` tool of its own. Zero advertises nothing.
    max_depth: u32,
    /// The stamped system prompt every inner agent runs under: the
    /// parent's full prompt, base template, skills, environment
    /// facts, user preferences, the subagent's own tool list,
    /// computed once at wire time off the config. Empty on direct
    /// constructions (tests): the body runs alone.
    base: String,
    /// Where a child session is saved and which session is its parent.
    /// `None` on a tool that has no store (tests, and the nested level
    /// inside a job): the inner session stays in memory, the way it
    /// behaved before child sessions existed.
    children: Arc<Mutex<Option<ChildStore>>>,
}

/// The store and parent identity a child session is written under.
#[derive(Clone)]
struct ChildStore {
    store: Arc<dyn SessionStore>,
    parent: SessionId,
}

/// One run's child session: the store to write it to, its id, and its
/// own turn record.
struct ChildRun {
    store: Arc<dyn SessionStore>,
    child: SessionId,
    parent: SessionId,
    turn: Option<rhema_submissions::SubmissionId>,
}

impl ChildRun {
    /// The engine's commit hook over this child: the same write, fired
    /// at every boundary the engine owns, so a crash mid-tool leaves
    /// the committed intent instead of the step before it.
    fn commit_hook(&self) -> rhema_core::CommitHook {
        let store = self.store.clone();
        let child = self.child.clone();
        Box::new(move |session: &Session| {
            store
                .save(&child, session)
                .map_err(|err| rhema_core::CommitError(err.to_string()))
        })
    }

    fn save(&self, session: &Session) -> Result<(), JobFailure> {
        self.store.save(&self.child, session).map_err(|err| {
            JobFailure::Permanent(format!("its child session could not be saved: {err}"))
        })
    }
}

/// Where one run rides: the provider its requests turn on and the
/// model id they carry, the resolved form of the call's `model`
/// argument over the definition's frontmatter over the session's own.
#[derive(Clone)]
struct Ride {
    provider: Arc<dyn ModelProvider>,
    model: String,
    /// The call's own spelling before resolution, so a restart or a
    /// child's descriptor aims the same way it did the first time.
    spelling: Option<String>,
}

impl Inner {
    /// The inner shell for one run: fresh, with `hook`, captured off
    /// the parent shell at spawn, installed. `None` means the inner
    /// tools reject: the fail-safe default, never the case mid-turn.
    fn inner_shell(
        &self,
        parent: &Arc<ShellTools>,
        hook: Option<Arc<dyn Approval>>,
    ) -> Arc<ShellTools> {
        let shell = Arc::new(
            ShellTools::with_executor(parent.workspace().to_path_buf(), parent.executor())
                // The parent's env names the kit init, the helper, and
                // the staging directory; the inbox inherits so the inner
                // commands' staged images attach to the inner results.
                .with_shell_env(&parent.shell_env())
                .with_vision_inbox(parent.vision_inbox()),
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
            // A nested run is part of its parent job's work, not a
            // child of the parent session: it keeps no session of its
            // own.
            children: Arc::new(Mutex::new(None)),
            ..self.clone()
        }
    }

    /// The child target a run writes to, when a store is wired.
    fn child_run(&self, child: Option<&SessionId>) -> Option<ChildRun> {
        let child = child?;
        let children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        Some(ChildRun {
            store: children.store,
            child: child.clone(),
            parent: children.parent,
            turn: None,
        })
    }

    /// Resolves where one run rides: the call's `model` argument wins
    /// over the definition's frontmatter, which wins over the parent's
    /// provider and model. The spelling splits on its first `/`: a
    /// first component naming a provider kind resolves through the
    /// factory, unresolvable is an error naming what is available,
    /// never a silent ride on the session's provider, while an
    /// unknown first component (an OpenRouter-style id, say) stays
    /// whole and rides the parent's provider. An explicit `<kind>/`
    /// with an empty tail names no model.
    fn resolve_model(&self, call_model: Option<&str>, def: &SubagentDef) -> Result<Ride, String> {
        let spelling = call_model.or(def.model.as_deref());
        let Some(spelling) = spelling else {
            return Ok(Ride {
                provider: self.provider.clone(),
                model: self.default_model.clone(),
                spelling: None,
            });
        };
        let spelling = spelling.trim();
        if let Some((kind, rest)) = spelling.split_once('/') {
            let known = rhema_config::PROVIDERS.iter().any(|info| info.name == kind);
            if known {
                let provider = self
                    .providers
                    .as_ref()
                    .and_then(|providers| providers(kind))
                    .ok_or_else(|| {
                        let available = if self.provider_kinds.is_empty() {
                            "none besides this session's own".to_owned()
                        } else {
                            self.provider_kinds.join(", ")
                        };
                        format!(
                            "provider kind \"{kind}\" has no usable credentials here \
                             (not logged in?); providers available to subagents: {available}"
                        )
                    })?;
                let model = rest.trim();
                if model.is_empty() {
                    return Err(format!(
                        "\"{spelling}\" names a provider but no model after the slash"
                    ));
                }
                return Ok(Ride {
                    provider,
                    model: model.to_owned(),
                    spelling: Some(spelling.to_owned()),
                });
            }
        }
        Ok(Ride {
            provider: self.provider.clone(),
            model: spelling.to_owned(),
            spelling: Some(spelling.to_owned()),
        })
    }

    /// Builds the inner agent one run rides: the stamped prompt with
    /// the definition's body, the resolver the caller built over the
    /// run's shell (the same resolver a continuation revives dangling
    /// calls through), and the provider wrapped in the job-tuned retry
    /// ladder.
    fn agent(
        &self,
        ride: &Ride,
        def: &SubagentDef,
        resolver: &Arc<dyn ToolResolver>,
        session_key: &str,
    ) -> Result<Agent, JobFailure> {
        // The inner system prompt: the stamp with this definition's
        // body appended verbatim. A body is plain text, never
        // rendered, template syntax in it reaches the model as the
        // author wrote it, so a definition written for another
        // harness behaves as written. An empty stamp (direct
        // constructions, tests) runs the body alone.
        let system = if self.base.is_empty() {
            def.body.clone()
        } else {
            format!("{}\n\n{}", self.base, def.body)
        };
        // Each run wraps the provider the caller resolved in its own
        // fresh retry and transcript: the wrapper holds no state
        // across requests, so a per-run instance is the shared one's
        // exact equal, and the inner history lives and dies with the
        // run, no mirror to share, no per-definition settings to
        // plumb. The `zoom` half of `attach` is dropped on purpose,
        // the subagent's tool surface stays the shell alone.
        let retrying = Arc::new(Retrying::with_ladders(
            ride.provider.clone(),
            self.network_ladder.clone(),
            self.server_ladder.clone(),
        ));
        let (retrying, _zoom) =
            rhema_transcript::attach(retrying, rhema_transcript::TranscriptSettings::default());
        Agent::builder()
            .provider(retrying)
            .resolver(resolver.clone())
            .system(system)
            .params(ChatParams {
                thinking: self.thinking,
                // The run's conversation key: a child's requests stick
                // to one provider bucket of their own, and the cache
                // the dialects opt into reads the child's prefix, not
                // the parent's.
                session_id: Some(session_key.to_owned()),
                ..ChatParams::new(ride.model.clone())
            })
            .build()
            .map_err(|err| JobFailure::Permanent(format!("its engine failed to build: {err}")))
    }

    /// Runs one definition to its report, or to the reason it failed.
    /// `ride` is the caller's resolution, this never consults the
    /// frontmatter or the default itself.
    ///
    /// With a child id, the run writes a real session: the descriptor
    /// and a `turn` record before the first model call, the history at
    /// every step boundary, and the settled record at the end. A kill
    /// anywhere in between leaves both, which is what the parent's
    /// `adopt` and `recover` read. A restart, the re-run of a job that
    /// failed transiently, loads that child first: the ledger is
    /// re-armed and the step loop continues the spent history where it
    /// stopped, nothing the failed run committed is thrown away.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        ride: &Ride,
        def: &SubagentDef,
        prompt: &str,
        ctx: &ToolContext,
        inner_shell: &Arc<ShellTools>,
        progress: &mut dyn ToolProgress,
        child: Option<&SessionId>,
    ) -> Result<String, JobFailure> {
        // The conversation key this run's requests carry: the
        // persisted child id when one exists (a restart resumes the
        // same id, so the retried conversation lands back on its
        // sticky bucket), the job's run shape otherwise (the nested
        // synchronous call runs without a store). Dialects that key
        // routing or caching on it read it; the rest ignore it.
        let session_key = child
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(|| format!("{}/subagent:{}", ctx.run_id, def.name));
        let resolver = self.inner_resolver(inner_shell);
        let mut agent = self.agent(ride, def, &resolver, &session_key)?;
        if let Some(target) = self.child_run(child) {
            agent.set_commit_hook(Some(target.commit_hook()));
        }
        let mut sink = Forwarder {
            progress,
            model: ride.model.clone(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        let inner_ctx = ToolContext {
            cwd: inner_shell.workspace().to_path_buf(),
            run_id: format!("{}/subagent:{}", ctx.run_id, def.name),
            cancelled: ctx.cancelled.clone(),
        };

        let mut child_run = self.child_run(child);
        // A restart reaches the child the first run already wrote: the
        // history it committed is the work it did, so the re-run
        // continues that session instead of building a fresh one over
        // it. The branch is strictly conditional on the load, and a
        // miss, the first run or a vanished file, keeps the fresh start
        // below; a store that cannot answer fails the job rather than
        // run blind over a file it could not read.
        let mut session = Session::default();
        let mut restart = false;
        if let Some(run) = &child_run {
            match run.store.load(&run.child) {
                Ok(Some(loaded)) => {
                    session = loaded;
                    restart = true;
                }
                Ok(None) => {}
                Err(err) => {
                    return Err(JobFailure::Permanent(format!(
                        "its child session could not be loaded: {err}"
                    )));
                }
            }
        }
        if restart {
            let run = child_run
                .as_mut()
                .expect("a restart only happens over a child run");
            kind::write_descriptor(
                &mut session,
                &kind::Descriptor {
                    def: def.name.clone(),
                    prompt: prompt.to_owned(),
                    model: ride.spelling.clone(),
                    parent: run.parent.clone(),
                },
            );
            run.turn = prepare_restart(&mut session, prompt, resolver.as_ref(), &inner_ctx).await?;
            run.save(&session)?;
        } else {
            if let Some(run) = child_run.as_mut() {
                kind::write_descriptor(
                    &mut session,
                    &kind::Descriptor {
                        def: def.name.clone(),
                        prompt: prompt.to_owned(),
                        model: ride.spelling.clone(),
                        parent: run.parent.clone(),
                    },
                );
                let at = rhema_submissions::now();
                if let Ok(record) = rhema_submissions::turn::submit(&mut session, prompt, at)
                    && let Ok(started) =
                        rhema_submissions::turn::start(&mut session, &record.id, prompt, at)
                {
                    run.turn = Some(started.id);
                }
            }
            session.history.push(Message::text(Role::User, prompt));
            if let Some(run) = &child_run {
                run.save(&session)?;
            }
        }

        let result = self
            .steps(
                &mut agent,
                &mut session,
                def,
                ctx,
                &inner_ctx,
                &mut sink,
                child_run.as_ref(),
            )
            .await;

        if let Some(run) = &child_run {
            if let Some(turn) = &run.turn {
                let outcome = match &result {
                    Ok(_) => rhema_submissions::Outcome::Done,
                    Err(failure) => rhema_submissions::Outcome::Failed {
                        error: failure.reason().to_owned(),
                        retryable: failure.is_transient(),
                    },
                };
                let _ = rhema_submissions::settle(
                    &mut session,
                    turn,
                    outcome,
                    rhema_submissions::now(),
                );
            }
            run.save(&session)?;
        }
        result
    }

    /// The step loop over one session, saving the child at each
    /// boundary when there is one.
    #[allow(clippy::too_many_arguments)]
    async fn steps(
        &self,
        agent: &mut Agent,
        session: &mut Session,
        def: &SubagentDef,
        ctx: &ToolContext,
        inner_ctx: &ToolContext,
        sink: &mut Forwarder<'_>,
        child: Option<&ChildRun>,
    ) -> Result<String, JobFailure> {
        for _ in 0..def.max_steps() {
            if ctx.cancelled.load(Ordering::Relaxed) {
                return Err(JobFailure::Permanent("cancelled".to_owned()));
            }
            let step = agent.step(session, sink, inner_ctx).await.map_err(|err| {
                // The failure class is decided where the error is
                // known: a retryable provider failure names the
                // infrastructure, everything else names the work.
                if err.is_retryable() {
                    JobFailure::Transient(err.to_string())
                } else {
                    JobFailure::Permanent(err.to_string())
                }
            })?;
            if let Some(child) = child {
                child.save(session)?;
            }
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

/// The index `prompt` occupies in the history, pushing it when it is
/// not already the last user message: a continuation never duplicates
/// the prompt, and the turn's `base` must read the message the work
/// actually starts from.
fn land_prompt(session: &mut Session, prompt: &str) -> u64 {
    if let Some((index, message)) = session
        .history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role == Role::User)
        && matches!(message.content.first(), Some(Content::Text(text)) if text == prompt)
    {
        return index as u64;
    }
    session.history.push(Message::text(Role::User, prompt));
    (session.history.len() - 1) as u64
}

/// What a loaded child needs before its re-run steps: its own turn
/// re-armed, or a new one when nothing is re-armable, and its dangling
/// calls revived. Returns the turn the run settles.
///
/// The newest `turn` record is the child's current work. When it names
/// this prompt and its status can go back to `Running`, it is re-armed
/// in place: the failed run settled it `Failed { retryable: true }`,
/// the one class a restart exists for, or a killed run left it for
/// `reconcile` to call `Interrupted`, and [`rhema_submissions::retry`]
/// takes both back to `Running`, keeping the id, the input it already
/// landed, and the history it already cost. A queued one starts where
/// its input now lands, the move [`SubagentTools::resume`] takes for
/// its states. Everything else, a settled `Done`, a
/// non-retryable failure, a record that names another prompt (a child
/// reached by misuse), submits a fresh turn over the same history and
/// lands the prompt once: the child keeps its spent history, the
/// tail never receives the prompt twice.
///
/// Dangling calls are revived either way, through the same resolver
/// the re-run steps through, after the approval hook is on the inner
/// shell: a replay-safe call re-executes under that approval, an
/// unsafe one lands the verify-state failure the engine's contract
/// spells, and nothing dangles when the step loop takes over.
async fn prepare_restart(
    session: &mut Session,
    prompt: &str,
    resolver: &dyn ToolResolver,
    ctx: &ToolContext,
) -> Result<Option<rhema_submissions::SubmissionId>, JobFailure> {
    let ledger = |err: rhema_submissions::SubmissionError| {
        JobFailure::Permanent(format!("its child ledger could not be read: {err}"))
    };
    // A stale `Running` record, a child left mid-run by a killed
    // process, first reads what its history says about it, the repair
    // resume opens with; a settled record is terminal and no kind is
    // asked.
    let kinds = rhema_submissions::Kinds::new().with(Arc::new(rhema_submissions::turn::Turn));
    rhema_submissions::reconcile(session, &kinds, rhema_submissions::now()).map_err(ledger)?;
    let at = rhema_submissions::now();
    let newest = rhema_submissions::snapshot(session)
        .map_err(ledger)?
        .iter()
        .rev()
        .find(|record| record.kind == rhema_submissions::turn::kind())
        .cloned();
    if let Some(record) = &newest
        && let Ok(turn) =
            serde_json::from_value::<rhema_submissions::turn::Payload>(record.payload.clone())
        && turn.input == prompt
    {
        let id = record.id.clone();
        let re_armed = match record.status {
            // One `retry` for both: the turn keeps its id and its
            // history, and `Running` is where the settle reads it. A
            // `Failed` record that lost its retryable flag refuses,
            // and the new-turn arm below takes over.
            rhema_submissions::Status::Failed | rhema_submissions::Status::Interrupted => {
                rhema_submissions::retry(session, &id, at).is_ok()
            }
            // The input never landed: start pins where it now will.
            rhema_submissions::Status::Queued => {
                let base = land_prompt(session, prompt);
                rhema_submissions::start(
                    session,
                    &id,
                    at,
                    Some(serde_json::json!({ "input": prompt, "base": base })),
                )
                .is_ok()
            }
            _ => false,
        };
        if re_armed {
            rhema_core::revive_dangling_calls(session, resolver, ctx).await;
            return Ok(Some(id));
        }
    }
    // No turn of this prompt can be re-armed: the child keeps its
    // history and gets a new one.
    let Ok(record) = rhema_submissions::turn::submit(session, prompt, at) else {
        return Ok(None);
    };
    let base = land_prompt(session, prompt);
    if rhema_submissions::start(
        session,
        &record.id,
        at,
        Some(serde_json::json!({ "input": prompt, "base": base })),
    )
    .is_err()
    {
        return Ok(None);
    }
    rhema_core::revive_dangling_calls(session, resolver, ctx).await;
    Ok(Some(record.id))
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
    /// Attaches the store a child session is saved under and the parent
    /// it belongs to. Called after wiring, when the frontend knows
    /// which conversation it opened; without it a run stays in memory.
    pub fn attach_store(&self, store: Arc<dyn SessionStore>, parent: SessionId) {
        *self
            .inner
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ChildStore { store, parent });
    }

    /// Reads a finished child's report out of the store, or reports the
    /// state it is stuck in. `None` when the id names no child.
    fn child_fetch(&self, id: &str) -> Option<Fetched> {
        let children = self
            .inner
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let child = SessionId::new(id).ok()?;
        let session = children.store.load(&child).ok().flatten()?;
        let descriptor = kind::descriptor(&session)?;
        if kind::finished(&session) {
            let report = kind::report(&session)?;
            return Some(Fetched::Report {
                name: descriptor.def,
                report,
            });
        }
        Some(Fetched::Interrupted {
            name: descriptor.def,
            child,
        })
    }

    /// Continues an interrupted child from its own session file: the
    /// explicit request a driver makes, never a load side effect.
    ///
    /// The child's ledger is reconciled first, so a record the dead
    /// process left `Running` becomes `Interrupted` or `Queued`
    /// through the turn kind's own rules. A queued record starts the
    /// child from its prompt; an interrupted one retries and continues
    /// the tail. The tail's dangling calls are revived through the run's
    /// own resolver, replay-safe ones re-run and unsafe ones land the
    /// verify-state failure, before the turn resumes. Both settle and
    /// save when the run returns.
    pub async fn resume(&self, child: &SessionId, ctx: &ToolContext) -> Result<String, String> {
        let children = self
            .inner
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "no child store is wired for this subagent tool".to_owned())?;
        let mut session = children
            .store
            .load(child)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| format!("child session {child} is gone"))?;
        let descriptor =
            kind::descriptor(&session).ok_or_else(|| format!("{child} is not a subagent child"))?;
        let def = kind::definition(&self.inner.defs, &descriptor.def)
            .ok_or_else(|| format!("no subagent named {}", descriptor.def))?;
        let ride = self
            .inner
            .resolve_model(descriptor.model.as_deref(), def)
            .map_err(|why| why.to_string())?;
        let inner_shell = self
            .inner
            .inner_shell(&self.shell, self.shell.current_approval());
        let resolver = self.inner.inner_resolver(&inner_shell);
        // The revived run keeps its child id: the resumed conversation
        // lands back on the sticky bucket the first run warmed.
        let mut agent = self
            .inner
            .agent(&ride, def, &resolver, child.as_str())
            .map_err(|failure| failure.reason().to_owned())?;

        // Classify what the dead process left, then repair the tail:
        // a dangling call is answered, a replay-safe one by re-running
        // it and an unsafe one by a failure telling the model to verify
        // state, so the model is never asked to answer a call that
        // never returned.
        let kinds = rhema_submissions::Kinds::new().with(Arc::new(rhema_submissions::turn::Turn));
        rhema_submissions::reconcile(&mut session, &kinds, rhema_submissions::now())
            .map_err(|err| err.to_string())?;
        let turn_id = rhema_submissions::snapshot(&session)
            .map_err(|err| err.to_string())?
            .iter()
            .rev()
            .find(|record| record.kind == rhema_submissions::turn::kind())
            .map(|record| (record.id.clone(), record.status));
        if session.history.is_empty() {
            session
                .history
                .push(Message::text(Role::User, &descriptor.prompt));
        }
        if let Some((id, status)) = &turn_id {
            let at = rhema_submissions::now();
            match status {
                rhema_submissions::Status::Queued => {
                    let payload = serde_json::json!({
                        "input": descriptor.prompt,
                        "base": 0,
                    });
                    rhema_submissions::start(&mut session, id, at, Some(payload))
                        .map_err(|err| err.to_string())?;
                }
                rhema_submissions::Status::Interrupted => {
                    rhema_submissions::retry(&mut session, id, at)
                        .map_err(|err| err.to_string())?;
                }
                _ => {}
            }
        }
        // Revive through the resolver the inner agent steps through,
        // after the approval hook is on that shell (a replay-safe call
        // re-runs under this turn's approval, the parent's ordering
        // rule) and before the turn resumes. One answer per dangling
        // call, the engine's own contract, so nothing is left dangling
        // and no close is needed after it.
        let inner_ctx = ToolContext {
            cwd: inner_shell.workspace().to_path_buf(),
            run_id: format!("{}/subagent:{}", ctx.run_id, def.name),
            cancelled: ctx.cancelled.clone(),
        };
        rhema_core::revive_dangling_calls(&mut session, resolver.as_ref(), &inner_ctx).await;
        agent.set_commit_hook(Some(
            ChildRun {
                store: children.store.clone(),
                child: child.clone(),
                parent: descriptor.parent.clone(),
                turn: turn_id.as_ref().map(|(id, _)| id.clone()),
            }
            .commit_hook(),
        ));
        children
            .store
            .save(child, &session)
            .map_err(|err| err.to_string())?;
        let mut silent = rhema_core::OutputOnly(|_| {});
        let mut sink = Forwarder {
            progress: &mut silent,
            model: ride.model.clone(),
            phase: None,
            context: None,
            spent: rhema_core::Usage::default(),
        };
        let result = agent.resume_turn(&mut session, &mut sink, &inner_ctx).await;
        let outcome = match &result {
            Ok(_) => rhema_submissions::Outcome::Done,
            Err(err) => rhema_submissions::Outcome::Failed {
                error: err.to_string(),
                retryable: err.is_retryable(),
            },
        };
        let failed = result.is_err();
        if let Some((id, _)) = &turn_id {
            let _ = rhema_submissions::settle(&mut session, id, outcome, rhema_submissions::now());
        }
        children
            .store
            .save(child, &session)
            .map_err(|err| err.to_string())?;
        if failed {
            return Err(result.expect_err("just checked").to_string());
        }
        kind::report(&session).ok_or_else(|| format!("{child} finished without a report"))
    }

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
                providers: None,
                provider_kinds: Vec::new(),
                network_ladder: JOB_NETWORK_LADDER.to_vec(),
                server_ladder: JOB_SERVER_LADDER.to_vec(),
                defs,
                default_model: default_model.into(),
                thinking: ThinkingEffort::Default,
                max_depth: 1,
                base: String::new(),
                children: Arc::new(Mutex::new(None)),
            },
            shell,
            registry: Some(Arc::new(Registry::new(
                crate::registry::DEFAULT_MAX_RUNNING,
            ))),
        }
    }

    /// Replaces the retry ladders a run's wrapper walks, the
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
    /// job, the notification path has one steering consumer, and a
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

    /// Installs the frontend's provider factory: the credential
    /// answers behind the `<provider>/<model>` spelling in the call's
    /// `model` argument and a definition's frontmatter. The kinds the
    /// factory resolves are probed once here, the provider list the
    /// spec teaches; a call resolves through the factory live, so a
    /// login mid-session still serves a job even though the prompt's
    /// list was stamped earlier.
    pub fn with_providers(mut self, providers: SubagentProviders) -> Self {
        self.inner.provider_kinds = rhema_config::PROVIDERS
            .iter()
            .map(|info| info.name)
            .filter(|kind| (providers)(kind).is_some())
            .collect();
        self.inner.providers = Some(providers);
        self
    }

    /// [`Self::with_providers`] over an optional factory: `None`
    /// installs nothing, the session's provider stays the only one.
    pub(crate) fn with_providers_if(self, providers: Option<SubagentProviders>) -> Self {
        match providers {
            Some(providers) => self.with_providers(providers),
            None => self,
        }
    }

    /// Sets the stamped system prompt the inner agents run under,
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
    /// subscription, and, through [`Registry`]'s [`Steering`](rhema_host_agent::Steering)
    /// implementation, the driver's notification source. The same
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
    /// change. `provider_kinds` is the providers a `model` argument can
    /// reach, the factory's wire-time answers.
    pub fn spec(defs: &[SubagentDef], provider_kinds: &[&str]) -> ToolSpec {
        Self::spec_for(defs, true, provider_kinds)
    }

    /// [`Self::spec`] per shape: the background spec carries the
    /// start/notification/fetch protocol; the synchronous (nested) one
    /// the report-as-result contract.
    fn spec_for(defs: &[SubagentDef], background: bool, provider_kinds: &[&str]) -> ToolSpec {
        let mut description = if background {
            String::from(
                "Run a named subagent in the background: an independent \
                 agent with its own conversation, system prompt, and \
                 workspace tools, which works the task and produces a \
                 final report. The subagent does not see this conversation \
                 and its report is all you get back, so the prompt must \
                 carry everything it needs. Call with `name` and `prompt` \
                 to start a subagent: the call returns a job id at once and \
                 the subagent keeps working in the background. When a job \
                 finishes you will receive a notification naming it; fetch \
                 its report by calling this tool with only `id` set to that \
                 job id (for example \"job-3\"). A fetch of a job that is \
                 still running is an error: wait for the notification. A \
                 finished job's report is cached: fetching again returns the \
                 same report, never a re-run. Jobs keep running across \
                 turns, and stopping this session never stops one.",
            )
        } else {
            String::from(
                "Run a named subagent: an independent agent with its own \
                 conversation, system prompt, and workspace tools, which \
                 works the task and returns its final report as the tool \
                 result. The subagent does not see this conversation and its \
                 report is all you get back, so the prompt must carry \
                 everything it needs.",
            )
        };
        if !defs.is_empty() {
            description.push_str("\n\nSubagents, by name:");
            for def in defs {
                description.push_str(&format!("\n- {}: {}", def.name, def.description));
            }
        }
        // Where a run rides. The definition's frontmatter and the
        // session's model are the defaults; the argument is the
        // override. The provider list is the factory's wire-time
        // answers, an empty list still leaves the bare-id spelling
        // meaningful, on this session's own provider.
        if provider_kinds.is_empty() {
            description.push_str(
                "\n\nWhere it runs: without `model`, the definition's own \
                 model choice applies, then this session's model. No other \
                 provider is available here, so a `<provider>/<model>` \
                 spelling fails with the provider named, and `model` takes \
                 a bare model id on this session's provider.",
            );
        } else {
            description.push_str(&format!(
                "\n\nWhere it runs: without `model`, the definition's own \
                 model choice applies, then this session's model. Pass \
                 `model` to move it: `<provider>/<model>` runs on another \
                 provider (available here: {}), and a bare model id runs on \
                 this session's provider.",
                provider_kinds.join(", ")
            ));
        }
        // The catalog also rides `name` as an enum, so a provider that
        // validates enums refuses a typo before the call. The two
        // argument sets are mutually exclusive, which the portable
        // schema subset cannot express as `oneOf`, so the field prose
        // carries it.
        let mut name_schema = json!({
            "type": "string",
            "description": "The subagent to run, from the catalog above. Required with `prompt`."
        });
        if !defs.is_empty() {
            name_schema["enum"] =
                json!(defs.iter().map(|def| def.name.as_str()).collect::<Vec<_>>());
        }
        let model_schema = json!({
            "type": "string",
            "description": "Optional. Moves the run: `<provider>/<model>` spends another provider's credentials where one is available here, a bare model id stays on this session's provider. Without it the definition's model applies, then this session's."
        });
        if background {
            ToolSpec::new(
                "subagent",
                description,
                json!({
                    "type": "object",
                    "properties": {
                        "name": name_schema,
                        "prompt": {
                            "type": "string",
                            "description": "The full task for the subagent: what to do, and what to report back. Required with `name`."
                        },
                        "id": {
                            "type": "string",
                            "description": "Fetch instead of starting: a job id from a notification or an earlier start. Pass it alone, never beside `name` or `prompt`."
                        },
                        "model": model_schema
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
                        "name": name_schema,
                        "prompt": {
                            "type": "string",
                            "description": "The full task for the subagent: what to do, and what to report back. Required with `name`."
                        },
                        "model": model_schema
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
        // A fetch carries no model: its arguments name a finished job,
        // and re-resolving a model a report already earned would be
        // re-running it by another name. An ignored stray `model`
        // beside `id` stays silent, the schema's Optional covers it.
        let model = call.args.get("model").and_then(|value| value.as_str());
        match (id, name, prompt) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(invalid(
                "pass `name` and `prompt` to start a subagent, or `id` to \
                 fetch a started one, not both"
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
                if let Some(model) = model
                    && model.trim().is_empty()
                {
                    return Err(invalid("the `model` argument is empty".to_owned()));
                }
                Ok(Parsed::Start { def, prompt, model })
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
        model: Option<&str>,
        ctx: &ToolContext,
    ) -> ToolResult {
        // Resolve once here so a spelling that cannot ride fails the
        // call outright, an id that starts a job destined to die is
        // not a start. The spawned task re-resolves the same spelling;
        // the factory's answers are live, and a restart re-aims the
        // same way.
        if let Err(why) = self.inner.resolve_model(model, def) {
            return ToolResult::failure(call_id.to_owned(), why);
        }
        let registry = self
            .registry
            .clone()
            .expect("the background path starts into a registry");
        let minted = registry.mint(&def.name);
        // The hook is read at spawn and held for the job's lifetime: a
        // gate firing after the parent turn's end still consults the
        // turn's policy, never the fail-safe reject. No hook installed
        // means the inner tools reject, the fail-safe default.
        let hook = self.shell.current_approval();
        // A child session is minted before the first model call, so
        // the record and the file cannot disagree about which session
        // the work is.
        let children = self
            .inner
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let child = children.as_ref().and_then(|children| {
            kind::child_id(
                children.store.as_ref(),
                &children.parent,
                rhema_submissions::now(),
            )
        });
        // No attached store mints no child; an attached one whose
        // parent id cannot carry the suffix fails the call in band:
        // every child's id rides its parent.
        if child.is_none() && children.is_some() {
            return ToolResult::failure(
                call_id.to_owned(),
                "the parent id is too long to mint a child: every child's id rides its parent"
                    .to_owned(),
            );
        }
        let rerun = JobRerun {
            def: def.clone(),
            prompt: prompt.to_owned(),
            parent_run: ctx.run_id.clone(),
            hook: hook.clone(),
            model: model.map(str::to_owned),
            child: child.clone(),
        };
        if let Some(child) = child {
            registry.announce_start(
                &minted.id,
                crate::registry::Started {
                    child,
                    def: def.name.clone(),
                    prompt: prompt.to_owned(),
                    model: model.map(str::to_owned),
                },
            );
        }
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
            // registry ends the wait with None, the agent is gone.
            let Some(permit) = crate::registry::acquire_slot(&weak_registry).await else {
                return;
            };
            // Re-resolve the spelling the call carried: the factory's
            // answers are live, so a re-run aims where the credentials
            // point now, and a definition's frontmatter rides the same
            // resolution it always had. A spelling that stopped
            // resolving (logged out since the start) fails the job
            // with the reason, fetchable, restartable.
            let ride = match inner.resolve_model(rerun.model.as_deref(), &rerun.def) {
                Ok(ride) => ride,
                Err(why) => {
                    if let Some(registry) = weak_registry.upgrade() {
                        registry.finish(&task_id, Err(JobFailure::Permanent(why)));
                    }
                    return;
                }
            };
            // Touch the registry only in short critical sections: the
            // task holds the Weak, never a strong ref across the run,
            // a dropped registry must actually drop, and its flag stops
            // the job at the next step boundary.
            if let Some(registry) = weak_registry.upgrade() {
                registry.mark_running(&task_id, Some(ride.model.clone()));
            }
            let ctx = ToolContext {
                cwd: inner_shell.workspace().to_path_buf(),
                run_id: format!("{}/{}/subagent:{}", rerun.parent_run, task_id, task_name),
                cancelled: cancel,
            };
            let mut progress = progress;
            let outcome = inner
                .run(
                    &ride,
                    &rerun.def,
                    &rerun.prompt,
                    &ctx,
                    &inner_shell,
                    &mut progress,
                    rerun.child.as_ref(),
                )
                .await;
            if let Some(registry) = weak_registry.upgrade() {
                registry.finish(&task_id, outcome);
            }
            drop(permit);
        });
    }

    /// Restarts every transient-failed job: the registry resets the
    /// records and hands back the ingredients, and this re-spawns each
    /// run under its own id. A re-run continues the child session the
    /// failed run left, its spent history intact and its turn re-armed
    /// in place, never a second start over the same work. The names
    /// come back for the frontend's notice; the re-runs announce
    /// themselves when they finish, into the same ids the model already
    /// knows.
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
    /// `name` + `prompt`: start a job over this definition, on the
    /// model the optional `model` argument names.
    Start {
        def: &'a SubagentDef,
        prompt: &'a str,
        model: Option<&'a str>,
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
        vec![Self::spec_for(
            &self.inner.defs,
            self.registry.is_some(),
            &self.inner.provider_kinds,
        )]
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
                    Fetched::Interrupted { name, child } => ToolResult::failure(
                        id,
                        format!(
                            "subagent {name} ({child}) is interrupted: its \
                             session is on disk and no process is driving \
                             it; resuming it is an explicit request, not a \
                             fetch"
                        ),
                    ),
                    Fetched::Unknown => match self.child_fetch(job) {
                        Some(fetched) => match fetched {
                            Fetched::Report { report, .. } => ToolResult::success(id, report),
                            Fetched::Failed { name, reason } => {
                                ToolResult::failure(id, format!("subagent {name}: {reason}"))
                            }
                            Fetched::Interrupted { name, child } => ToolResult::failure(
                                id,
                                format!(
                                    "subagent {name} ({child}) is interrupted: its \
                                     session is on disk and no process is \
                                     driving it; resuming it is an explicit \
                                     request, not a fetch"
                                ),
                            ),
                            Fetched::Busy { name } => ToolResult::failure(
                                id,
                                format!("subagent {name} ({job}) is still running"),
                            ),
                            Fetched::Unknown => {
                                ToolResult::failure(id, format!("no such child session: {job}"))
                            }
                        },
                        None => ToolResult::failure(
                            id,
                            format!(
                                "no such subagent job: {job}; a job lives in the \
                                 agent's memory, and a finished child's report \
                                 lives in its session file"
                            ),
                        ),
                    },
                }
            }
            Ok(Parsed::Start { def, prompt, model }) if self.registry.is_some() => {
                self.start(&id, def, prompt, model, ctx)
            }
            Ok(Parsed::Start { def, prompt, model }) => {
                // The synchronous (nested) shape: run to the report
                // inside the resolving step, the hook captured here as
                // anywhere.
                let outcome = match self.inner.resolve_model(model, def) {
                    Ok(ride) => {
                        let hook = self.shell.current_approval();
                        let inner_shell = self.inner.inner_shell(&self.shell, hook);
                        let outcome = self
                            .inner
                            .run(&ride, def, prompt, ctx, &inner_shell, progress, None)
                            .await;
                        inner_shell.clear_approval();
                        outcome
                    }
                    Err(why) => Err(JobFailure::Permanent(why)),
                };
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
/// UI renders. The inner output rides no channel, the job is its own
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
/// turn's text and tool output go nowhere, the subagent is
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
    /// next call's first frame replaces it, the reply rides into the
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
            // report per announcement, the attempt number moves, and
            // the next delta returns the flow phases.
            Event::RetryWait { attempt, total, .. } => {
                self.report(format!("retrying model call (attempt {attempt}/{total})"));
            }
            // A nested subagent's status stays private to it, argument
            // streaming is not a phase of its own, and the inner
            // output's lines ride no channel. A running tally replaces
            // the context, it is the current call's figure, never a
            // spend to accumulate; the accounting still rides `StepEnd`.
            Event::ToolStatus { .. }
            | Event::ToolCallArgsDelta { .. }
            | Event::ToolOutputDelta { .. } => {}
            Event::Usage(usage) if self.context.as_ref() != Some(&usage) => {
                self.context = Some(usage);
                self.send();
            }
            // The enum grows across releases; a phase-less event
            // moves no report.
            _ => {}
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
mod tests;
