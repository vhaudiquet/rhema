//! The job registry: everything a started subagent becomes.
//!
//! A start call mints a job id and registers it here
//! (`queued → running → finished(ok, report) | finished(error,
//! reason)`); the report is cached at production, capped, and a fetch
//! only reads it back, a double-fetch returns the cache, never a
//! re-run, because subagents are [`ReplaySafety::Unsafe`](rhema_core::ReplaySafety)
//! and the fetch is a read of this registry, not an execution. Each
//! job carries its own cancel flag, minted at spawn, never the parent
//! turn's, so escape stops the main session and running subagents do
//! not. Dropping the registry is the only stop switch: it flips every
//! job's flag, queued jobs never start, and running ones end at their
//! next step boundary, the jobs are bound to the agent's lifetime,
//! and their reports are lost with it.
//!
//! Concurrency: at most `max_running` top-level jobs run
//! at once; a start beyond the cap queues, the id is still minted and
//! returned at once, and the job begins when a slot frees.
//!
//! Three surfaces leave the registry. [`Registry::status`] is the
//! subscription the UI's redraw tick renders one working line per job
//! from, working only: a finished job leaves it at once, and the
//! finish moves to the transcript as a notice. The [`Steering`]
//! implementation is one notification line per finished-but-unfetched
//! job, a pure query, the driver deduplicates within a turn, and a
//! crash between its append and its checkpoint only re-delivers. And
//! the two consuming takes serve the frontend: `take_finished`
//! delivers each finish once, the transcript notice, and
//! `take_notifications` hands a session with no running
//! turn the pending lines to run as the next turn's prompt, retiring
//! them so the driver's drain does not repeat what the prompt already
//! said.
//!
//! Failures split into two classes, and the class decides what a
//! restart may do. A [`JobFailure::Transient`] names the
//! infrastructure, the model call died on the network or the server,
//! the same class the provider's retry ladder waits out, and the job
//! carried everything it needs to run again, so the user's retry
//! command restarts it in place: `restart_failed` moves
//! every transient-failed job back to queued under its own id, with a
//! fresh cancel flag and clock, and hands the spawn ingredients back
//! to the tool. The re-run continues the same child session: what the
//! failed run committed stays, its turn re-armed in place rather than
//! re-submitted, and the work picks up where it stopped instead of
//! starting over. The pending failure announcements retire with the
//! state they describe, the report cache clears, and the re-run
//! finishes into the same id, the model sees the failure, then a
//! finish, and fetches as it always did. A
//! [`JobFailure::Permanent`] names the work itself, no report, a
//! spent step budget, and never restarts: re-running it repeats the
//! failure at full cost.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rhema_core::Usage;
use rhema_host_agent::Steering;
use rhema_session_store::SessionId;
use rhema_submissions::Outcome;

/// How many top-level jobs run at once when the constructor knob is
/// not set.
pub const DEFAULT_MAX_RUNNING: usize = 4;

/// How often a queued job polls for a freed slot. A job's start is not
/// latency-critical, its report is announced when it finishes, and a
/// poll keeps the wait independent of the registry's lifetime: a
/// dropped registry is observed as a failed upgrade, never a missed
/// wakeup.
const SLOT_POLL: Duration = Duration::from_millis(20);

/// One job's screen-facing state: what the UI's pinned live line
/// renders. The same [`rhema_core::ToolStatus`] vocabulary the
/// forwarder reports for a synchronous call.
#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    /// The job's registry id (`job-<n>`).
    pub id: String,
    /// The subagent definition the job runs.
    pub name: String,
    /// `queued`, or the phase the inner turn last reported. A finished
    /// job is not in the snapshot: the transcript carries the outcome.
    pub phase: String,
    /// The job's current model call's usage, when reported.
    pub context: Option<Usage>,
    /// The job's cumulative token spend, when reported.
    pub spent: Option<Usage>,
    /// The model the job runs on, when known.
    pub model: Option<String>,
    /// When the start call minted the job: the clock the line's age
    /// reads.
    pub started_at: Instant,
}

/// What a fetch of one job's report found.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// The job finished ok; the cached report.
    Report {
        /// The subagent definition the job ran.
        name: String,
        /// The cached report.
        report: String,
    },
    /// The job finished with an error; the reason.
    Failed {
        /// The subagent definition the job ran.
        name: String,
        /// Why it failed.
        reason: String,
    },
    /// The job is queued or running; no report exists yet.
    Busy {
        /// The subagent definition the job runs.
        name: String,
    },
    /// The child session exists, but its work is unfinished and no
    /// process is driving it. Fetching never starts it: a resume is an
    /// explicit request.
    Interrupted {
        /// The subagent definition the child runs.
        name: String,
        /// The child session a resume would take.
        child: rhema_session_store::SessionId,
    },
    /// No job by that id, one the model invented, or one a restart
    /// lost (the registry is in-memory).
    Unknown,
}

/// Why a job ended without a report, and whether a retry could clear
/// it. The class is decided where the failure is known, the inner
/// turn's step error, and rides the finish, so a restart never has to
/// guess from the reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFailure {
    /// The infrastructure failed: a dead link, a transport timeout, a
    /// struggling server, every [`ProviderError`](rhema_core::ProviderError) a retry ladder
    /// would wait out. The job may run again; the user's retry
    /// command restarts it, and the re-run continues the child session
    /// the failed run left, its spent history intact, from where its
    /// work stopped.
    Transient(String),
    /// The work itself failed: no report was written, or the step
    /// budget ran out. Re-running repeats the failure at full cost,
    /// so the job never restarts; the model reads the reason and may
    /// delegate differently.
    Permanent(String),
}

impl JobFailure {
    /// The failure worded for the model and the transcript.
    pub fn reason(&self) -> &str {
        match self {
            JobFailure::Transient(reason) | JobFailure::Permanent(reason) => reason,
        }
    }

    /// Whether the failure is one `restart_failed` picks
    /// up.
    pub fn is_transient(&self) -> bool {
        matches!(self, JobFailure::Transient(_))
    }
}

/// What a job keeps so the user's retry command can run it again: the
/// spawn ingredients, captured at start and held for the job's life.
/// The tool reads them back on a restart; the record never re-runs a
/// job on its own.
#[derive(Clone)]
pub struct JobRerun {
    /// The subagent definition the job runs.
    pub(crate) def: crate::def::SubagentDef,
    /// The stamped parent prompt the job was started with.
    pub(crate) prompt: String,
    /// The parent turn's run id the job's own run id was built from.
    pub(crate) parent_run: String,
    /// The approval hook captured off the parent shell at spawn, held
    /// for the job's lifetime.
    pub(crate) hook: Option<std::sync::Arc<dyn rhema_bash::Approval>>,
    /// The call's raw `model` argument, the same spelling the call
    /// carried; a restart re-resolves it against the factory, so a
    /// re-run lands where the original was aimed, not where a
    /// wire-time snapshot put it. `None` inherits the definition's
    /// frontmatter, then the parent's model.
    pub(crate) model: Option<String>,
    /// The child session this job writes to, when a store is wired.
    /// A restart keeps it, so the re-run continues the same child.
    pub(crate) child: Option<rhema_session_store::SessionId>,
}

impl std::fmt::Debug for JobRerun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The hook is a dyn seam; the ingredients that name the run
        // print in full.
        f.debug_struct("JobRerun")
            .field("def", &self.def.name)
            .field("prompt", &self.prompt)
            .field("parent_run", &self.parent_run)
            .field("hook", &self.hook.as_ref().map(|_| "<dyn Approval>"))
            .field("model", &self.model)
            .field("child", &self.child)
            .finish()
    }
}

/// One job [`Registry::restart_failed`] moved back to queued, with
/// everything the tool needs to spawn its re-run.
#[derive(Debug)]
pub(crate) struct RestartJob {
    /// The job's registry id, kept across the restart.
    pub id: String,
    /// The subagent definition the job runs.
    pub name: String,
    /// The spawn ingredients captured at start.
    pub rerun: JobRerun,
    /// The fresh cancel flag the re-run's task owns.
    pub cancel: Arc<AtomicBool>,
}

/// A job the tool restarted, as the frontend's notice names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restarted {
    /// The job's registry id, kept across the restart.
    pub id: String,
    /// The subagent definition the job runs.
    pub name: String,
}

/// One finished job, as the finish feed delivers it: the transcript
/// notice a frontend pushes the moment a job ends. The `line` is the
/// same announcement the driver steers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFinished {
    /// The job's registry id (`job-<n>`).
    pub id: String,
    /// The subagent definition the job ran.
    pub name: String,
    /// The outcome in the notification wording: the job finished, or
    /// finished with an error and its reason.
    pub line: String,
}

/// One start the parent's ledger has not recorded yet: the child
/// session and the recipe a record needs. A tool cannot write the
/// parent session, so the frontend drains these at its next commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// The child session the work writes to.
    pub child: rhema_session_store::SessionId,
    /// The subagent definition it runs.
    pub def: String,
    /// What the parent asked.
    pub prompt: String,
    /// The model spelling the call carried.
    pub model: Option<String>,
}

/// A minted job: its id and the cancel flag the spawned task owns.
#[derive(Debug, Clone)]
pub(crate) struct Minted {
    /// The registry id (`job-<n>`).
    pub id: String,
    /// The job's own cancel flag: flipped when the registry drops,
    /// never by the parent turn's escape.
    pub cancel: Arc<AtomicBool>,
}

/// The state machine one job moves through.
#[derive(Debug)]
enum State {
    /// Registered, waiting for a concurrency slot.
    Queued,
    /// Its task holds a slot and the inner turn is working.
    Running {
        phase: String,
        context: Option<Usage>,
        spent: Option<Usage>,
        model: Option<String>,
    },
    /// Its report exists (capped at production) and stays cached until
    /// the registry drops or [`MAX_RETAINED_JOBS`] evicts it. `fetched`
    /// retires the announcements (the cache stays until eviction);
    /// `announced` retires the notification line for a frontend that
    /// took it to run as a prompt.
    Finished {
        outcome: Result<String, JobFailure>,
        fetched: bool,
        announced: bool,
    },
}

/// One registered job.
#[derive(Debug)]
struct Job {
    name: String,
    /// The child session the job writes to, known once the start is
    /// announced: the retirements queue the ledger settle for it.
    /// `None` before that, and for a start the registry never saw.
    child: Option<SessionId>,
    /// The mint order: status lines and notifications come out oldest
    /// first, not lexicographically by id.
    seq: u64,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
    state: State,
    /// The job's cumulative spend already folded into the registry's
    /// pending total. A repeated status bills nothing more; a restart
    /// clears it, the re-run's spend starts from zero.
    billed: Option<Usage>,
    /// The spawn ingredients a restart needs; armed at start, kept
    /// across restarts so a re-run that fails transiently can restart
    /// again.
    rerun: Option<JobRerun>,
}

/// The jobs of one agent: ids, states, the report cache, the cap.
pub struct Registry {
    inner: Mutex<RegistryInner>,
    permits: Arc<tokio::sync::Semaphore>,
}

/// The most jobs one registry retains, working or finished. Finished
/// reports are the unbounded part: a session that delegates every few
/// turns would keep every report for its whole life. When the cap is
/// crossed, the oldest fetched report goes first, then the oldest
/// finished one. Working jobs are never evicted.
const MAX_RETAINED_JOBS: usize = 20;

/// The most finished-but-untaken events one registry holds. The
/// frontend drains [`Registry::take_finished`] every frame; the cap
/// is the guard for a frontend that stops reading, each event names
/// a finished job, so unbounded growth here is unbounded retention.
const MAX_FINISHED: usize = 128;

#[derive(Debug)]
struct RegistryInner {
    next: u64,
    jobs: BTreeMap<String, Job>,
    /// Finished-but-untaken jobs, oldest first: the finish feed
    /// [`Registry::take_finished`] drains. Capped at [`MAX_FINISHED`];
    /// the overflow is counted and reported as a synthetic event.
    finished: Vec<JobFinished>,
    /// Finished events dropped for nobody reading fast enough.
    dropped: usize,
    /// Starts not yet drained by the frontend: the parent ledger's
    /// writes, waiting for a session it can write.
    starts: Vec<Started>,
    /// Notification lines for finishes the registry did not observe: a
    /// child that ended while the parent was down, adopted at load. They
    /// ride the steering drain or the idle take, whichever lands first.
    extra: Vec<String>,
    /// Children whose finishes the registry retired, oldest first: the
    /// delivery (a prompt line taken) or the consumption (a report
    /// fetched) that lets the ledger's record settle. Drained by the
    /// frontend beside its session writes.
    settled: Vec<Settled>,
    /// Spend the frontend has not priced yet, per model id: each job's
    /// growth since its last billed status. Drained by
    /// [`Registry::take_spend`].
    spend: BTreeMap<String, Usage>,
}

impl Registry {
    /// A registry running at most `max_running` top-level jobs at
    /// once; starts past it queue.
    pub fn new(max_running: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next: 0,
                jobs: BTreeMap::new(),
                finished: Vec::new(),
                dropped: 0,
                starts: Vec::new(),
                extra: Vec::new(),
                settled: Vec::new(),
                spend: BTreeMap::new(),
            }),
            permits: Arc::new(tokio::sync::Semaphore::new(max_running.max(1))),
        }
    }

    /// A concurrency slot, when one is free. A queued job's task polls
    /// this until it takes one, watching the registry stay alive.
    pub(crate) fn try_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// Mints a job: the id is returned at once and the job sits queued
    /// until its task takes a slot. The cancel flag is the job's own,
    /// the caller's turn flag never reaches it.
    pub(crate) fn mint(&self, name: &str) -> Minted {
        let mut inner = self.lock();
        inner.next += 1;
        let seq = inner.next;
        let id = format!("job-{seq}");
        let cancel = Arc::new(AtomicBool::new(false));
        inner.jobs.insert(
            id.clone(),
            Job {
                name: name.to_owned(),
                child: None,
                seq,
                cancel: cancel.clone(),
                started_at: Instant::now(),
                state: State::Queued,
                billed: None,
                rerun: None,
            },
        );
        Minted { id, cancel }
    }

    /// Records a start the parent's ledger has not written: the child
    /// session and the recipe a record needs, and the child against
    /// the job for the retirements' settles. Drained by the frontend
    /// at its next commit, and redundant with `adopt` if it never is.
    pub(crate) fn announce_start(&self, id: &str, started: Started) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(id) {
            job.child = Some(started.child.clone());
        }
        inner.starts.push(started);
    }

    /// Takes the pending starts out, oldest first. The frontend submits
    /// each one into the parent's ledger, then saves once.
    pub fn take_starts(&self) -> Vec<Started> {
        std::mem::take(&mut self.lock().starts)
    }

    /// Notes a finish the registry did not observe: a child that ended
    /// while the parent was down. It reaches the transcript take and
    /// the notification feed, once each, like a live finish.
    pub fn note_finished(&self, finished: JobFinished) {
        let mut inner = self.lock();
        inner.extra.push(finished.line.clone());
        inner.finished.push(finished);
    }

    /// Arms a minted job for a later restart: the spawn ingredients a
    /// [`Self::restart_failed`] hands back. A start calls this right
    /// after minting; a job never armed (none today, every start
    /// arms) simply never restarts.
    pub(crate) fn arm_restart(&self, id: &str, rerun: JobRerun) {
        if let Some(job) = self.lock().jobs.get_mut(id) {
            job.rerun = Some(rerun);
        }
    }

    /// The job's task took a slot: the inner turn starts working.
    pub(crate) fn mark_running(&self, id: &str, model: Option<String>) {
        if let Some(job) = self.lock().jobs.get_mut(id)
            && matches!(job.state, State::Queued)
        {
            job.state = State::Running {
                phase: "thinking".to_owned(),
                context: None,
                spent: None,
                model,
            };
        }
    }

    /// Folds one [`rhema_core::ToolStatus`] from the job's inner turn
    /// into its record. Only a running job changes: a finished one
    /// keeps its last state. A spend figure adds its growth since the
    /// job's last report to the pending total, so a frontend can price
    /// what the delegate's steps billed.
    pub(crate) fn report(&self, id: &str, status: &rhema_core::ToolStatus) {
        let mut inner = self.lock();
        let mut billed = None;
        if let Some(job) = inner.jobs.get_mut(id) {
            let previous = job.billed.unwrap_or_default();
            if let State::Running {
                phase,
                context,
                spent,
                model,
            } = &mut job.state
            {
                *phase = status.phase.clone();
                *context = status.context;
                if let Some(total) = status.spent {
                    let delta = Usage {
                        input_tokens: total.input_tokens.saturating_sub(previous.input_tokens),
                        output_tokens: total.output_tokens.saturating_sub(previous.output_tokens),
                        cached_input_tokens: total
                            .cached_input_tokens
                            .saturating_sub(previous.cached_input_tokens),
                    };
                    *spent = Some(total);
                    job.billed = Some(total);
                    if delta != Usage::default() {
                        let model = status.model.clone().or_else(|| model.clone());
                        billed = Some((model.unwrap_or_default(), delta));
                    }
                }
                if status.model.is_some() {
                    *model = status.model.clone();
                }
            }
        }
        if let Some((model, delta)) = billed {
            let total = inner.spend.entry(model).or_default();
            total.input_tokens = total.input_tokens.saturating_add(delta.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(delta.output_tokens);
            total.cached_input_tokens = total
                .cached_input_tokens
                .saturating_add(delta.cached_input_tokens);
        }
    }

    /// The job's run ended: its report or the classified reason it
    /// failed. The report was capped at production; the cache is what
    /// was cut.
    pub(crate) fn finish(&self, id: &str, outcome: Result<String, JobFailure>) {
        let mut inner = self.lock();
        let Some(job) = inner.jobs.get_mut(id) else {
            return;
        };
        if matches!(job.state, State::Finished { .. }) {
            return;
        }
        let name = job.name.clone();
        let line = announcement(&name, id, &outcome);
        job.state = State::Finished {
            outcome,
            fetched: false,
            announced: false,
        };
        trim_finished_jobs(&mut inner);
        while inner.finished.len() >= MAX_FINISHED {
            let _ = inner.finished.remove(0);
            inner.dropped += 1;
        }
        inner.finished.push(JobFinished {
            id: id.to_owned(),
            name,
            line,
        });
    }

    /// Reads a job's report back, never an execution. A Report or
    /// Failed fetch marks the job fetched: it stops announcing here
    /// and in the steering drain, while the cache stays until the
    /// registry drops.
    pub(crate) fn fetch(&self, id: &str) -> Fetched {
        let mut inner = self.lock();
        let Some(job) = inner.jobs.get_mut(id) else {
            return Fetched::Unknown;
        };
        let fetched_state = match &job.state {
            State::Queued | State::Running { .. } => Fetched::Busy {
                name: job.name.clone(),
            },
            State::Finished { outcome, .. } => match outcome {
                Ok(report) => Fetched::Report {
                    name: job.name.clone(),
                    report: report.clone(),
                },
                Err(failure) => Fetched::Failed {
                    name: job.name.clone(),
                    reason: failure.reason().to_owned(),
                },
            },
        };
        // The fetch retires the job: its UI line leaves, its
        // announcements stop, and the consumption queues the settle
        // unless the delivery retired the job first, the announced
        // flag says which came first. A Busy fetch retires nothing.
        let mut queue = false;
        if !matches!(fetched_state, Fetched::Busy { .. })
            && let Some(job) = inner.jobs.get_mut(id)
            && let State::Finished {
                announced, fetched, ..
            } = &mut job.state
        {
            queue = !*announced;
            *fetched = true;
        }
        if queue {
            queue_settle(&mut inner, id);
        }
        fetched_state
    }

    /// The notification lines: one per finished job neither fetched
    /// nor taken, oldest first. A pure query, calling it twice
    /// returns the same lines; the driver deduplicates within a turn,
    /// and a line a frontend took for its prompt is retired at the
    /// take, not here.
    /// The notification lines without consuming them: the tests read
    /// the feed twice to assert the steering stays a query.
    #[cfg(test)]
    pub(crate) fn notifications(&self) -> Vec<String> {
        let inner = self.lock();
        let mut lines = notification_lines(&inner);
        lines.extend(inner.extra.iter().cloned());
        lines
    }

    /// Takes the pending notification lines out, oldest first, marking
    /// each taken job announced: its line stops announcing here and in
    /// the [`Steering`] drain, and the retirement queues the settle
    /// [`take_settled`] hands the frontend, a delivered finish is the
    /// point the ledger's record may stop saying `Running`. The
    /// frontend's idle path, a session with no running turn runs the
    /// lines as the next turn's prompt, and this take keeps the
    /// driver's turn-start drain from repeating what the prompt
    /// already said. While a turn runs, the lines belong to the
    /// driver's steering: do not call this.
    pub(crate) fn take_notifications(&self) -> Vec<String> {
        let mut inner = self.lock();
        let mut taken: Vec<(u64, String, String)> = inner
            .jobs
            .iter()
            .filter_map(|(id, job)| match &job.state {
                State::Finished {
                    outcome,
                    fetched: false,
                    announced: false,
                } => Some((job.seq, id.clone(), announcement(&job.name, id, outcome))),
                _ => None,
            })
            .collect();
        taken.sort_by_key(|(seq, _, _)| *seq);
        for (_, id, _) in &taken {
            if let Some(job) = inner.jobs.get_mut(id)
                && let State::Finished { announced, .. } = &mut job.state
            {
                *announced = true;
                queue_settle(&mut inner, id);
            }
        }
        let mut lines: Vec<String> = taken.into_iter().map(|(_, _, line)| line).collect();
        lines.append(&mut inner.extra);
        lines
    }

    /// Takes the children whose finishes the registry retired — each
    /// line a prompt delivered, or a report the model fetched — with
    /// the ledger outcome the record settles to. The frontend drains
    /// this beside its session writes: a finish left `Running` in the
    /// ledger reads at every future load as one the process died
    /// before it answered, and a resumed session re-announces it as
    /// new. Empty after a drain.
    pub fn take_settled(&self) -> Vec<Settled> {
        std::mem::take(&mut self.lock().settled)
    }

    /// Takes the spend the frontend has not priced yet: per model id,
    /// the tokens every job's steps added since the previous take. A
    /// frontend prices each entry by that model's card and adds the
    /// result to the session's cost. Empty after a drain.
    pub fn take_spend(&self) -> Vec<(String, Usage)> {
        std::mem::take(&mut self.lock().spend).into_iter().collect()
    }

    /// Takes the finish events out, oldest first, one per job,
    /// exactly once. The transcript notice feed: a frontend pushes
    /// each event's `line` when the job ends, whatever turn is
    /// running. Taking an event retires nothing else: the job keeps
    /// announcing until its report is fetched or its line is taken.
    pub(crate) fn take_finished(&self) -> Vec<JobFinished> {
        let mut inner = self.lock();
        let mut events = std::mem::take(&mut inner.finished);
        events.sort_by_key(|event| inner.jobs.get(&event.id).map_or(0, |job| job.seq));
        if inner.dropped > 0 {
            // The overflow speaks once, in the feed's own voice: a
            // model reading it knows some outcomes never arrived.
            events.insert(
                0,
                JobFinished {
                    id: String::new(),
                    name: String::new(),
                    line: format!(
                        "{} older subagent notice{} dropped, nobody was \
                         reading them",
                        inner.dropped,
                        if inner.dropped == 1 { "" } else { "s" }
                    ),
                },
            );
            inner.dropped = 0;
        }
        events
    }

    /// Moves every transient-failed job back to queued, in mint
    /// order, and hands the spawn ingredients back: the tool re-spawns
    /// what this returns. A restart keeps the job's id and mint order,
    /// refreshes the clock, and mints a fresh cancel flag, the old
    /// task is gone, so the re-run's task owns its own. Leaving
    /// `Finished` retires whatever was pending on it: the failure's
    /// notification line (it described a state that no longer exists,
    /// and a model that read it would delegate the same task beside
    /// the re-run), and the report cache (a fetch now reads "still
    /// running"). Finish events already in the feed stay, the failure
    /// happened, the transcript may have shown it. Ok jobs and
    /// permanent failures stay put; a re-run that fails transiently
    /// again keeps its ingredients and restarts again.
    pub(crate) fn restart_failed(&self) -> Vec<RestartJob> {
        let mut inner = self.lock();
        let mut ids: Vec<String> = inner
            .jobs
            .iter()
            .filter(|(_, job)| {
                matches!(
                    &job.state,
                    State::Finished {
                        outcome: Err(failure),
                        ..
                    } if failure.is_transient()
                )
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort_by_key(|id| inner.jobs.get(id).map(|job| job.seq).unwrap_or(0));
        ids.into_iter()
            .filter_map(|id| {
                let job = inner.jobs.get_mut(&id)?;
                let rerun = job.rerun.clone()?;
                let cancel = Arc::new(AtomicBool::new(false));
                job.cancel = cancel.clone();
                job.started_at = Instant::now();
                job.state = State::Queued;
                job.billed = None;
                Some(RestartJob {
                    id,
                    name: job.name.clone(),
                    rerun,
                    cancel,
                })
            })
            .collect()
    }

    /// The subscription the UI renders: one entry per working job,
    /// queued or running. A finished job leaves the snapshot at once
    /// (the finish notice carries the outcome) and its report stays on
    /// the fetch until the registry drops or the retention cap evicts
    /// it.
    pub fn status(&self) -> Vec<JobStatus> {
        let inner = self.lock();
        let mut jobs: Vec<(&str, &Job)> = inner
            .jobs
            .iter()
            .filter(|(_, job)| !matches!(job.state, State::Finished { .. }))
            .map(|(id, job)| (id.as_str(), job))
            .collect();
        jobs.sort_by_key(|(_, job)| job.seq);
        jobs.iter()
            .map(|(id, job)| {
                let (phase, context, spent, model) = match &job.state {
                    State::Queued => ("queued".to_owned(), None, None, None),
                    State::Running {
                        phase,
                        context,
                        spent,
                        model,
                    } => (phase.clone(), *context, *spent, model.clone()),
                    State::Finished { .. } => unreachable!("filtered to working jobs"),
                };
                JobStatus {
                    id: (*id).to_owned(),
                    name: job.name.clone(),
                    phase,
                    context,
                    spent,
                    model,
                    started_at: job.started_at,
                }
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Drops finished jobs until the map is within [`MAX_RETAINED_JOBS`].
/// A fetched report is dead weight once the caller read it, so those
/// go first; if every finished job is still unread, the oldest goes
/// anyway. A working job is never a candidate.
fn trim_finished_jobs(inner: &mut RegistryInner) {
    while inner.jobs.len() > MAX_RETAINED_JOBS {
        let fetched = inner
            .jobs
            .iter()
            .filter_map(|(id, job)| match job.state {
                State::Finished { fetched: true, .. } => Some((job.seq, id.clone())),
                _ => None,
            })
            .min_by_key(|(seq, _)| *seq);
        let victim = fetched.or_else(|| {
            inner
                .jobs
                .iter()
                .filter_map(|(id, job)| match job.state {
                    State::Finished { .. } => Some((job.seq, id.clone())),
                    _ => None,
                })
                .min_by_key(|(seq, _)| *seq)
        });
        let Some((_, id)) = victim else {
            break;
        };
        inner.jobs.remove(&id);
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // The agent is gone: running jobs stop at their next step
        // boundary, queued ones never start. Their reports are lost
        // with the registry, the session never carried them.
        for job in self.lock().jobs.values() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// One finished child the frontend's ledger settles: the delivery of
/// a finish — a prompt line taken, a report fetched — is the point the
/// record may stop saying `Running`, because the model answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The child session the record's payload names.
    pub child: SessionId,
    /// The outcome `settle` writes: a report is `Done`, a failure
    /// carries its reason and the restart ladder's own class.
    pub outcome: Outcome,
}

/// Queues the settle the retirement of a finished job owes its ledger
/// record: the child the start announced, with the outcome the settle
/// writes. Nothing for a job without a child — its work was never
/// recorded, so nothing settles. Called after the retiring flag
/// flipped, from exactly one retirement per finish.
fn queue_settle(inner: &mut RegistryInner, id: &str) {
    let settle = match inner.jobs.get(id) {
        Some(Job {
            child: Some(child),
            state: State::Finished { outcome, .. },
            ..
        }) => Some(Settled {
            child: child.clone(),
            outcome: match outcome {
                Ok(_) => Outcome::Done,
                Err(failure) => Outcome::Failed {
                    error: failure.reason().to_owned(),
                    retryable: failure.is_transient(),
                },
            },
        }),
        _ => None,
    };
    if let Some(settle) = settle {
        inner.settled.push(settle);
    }
}

/// The notification line for one finished job: the driver's steering
/// vocabulary, and the finish notice's text.
fn announcement(name: &str, id: &str, outcome: &Result<String, JobFailure>) -> String {
    match outcome {
        Ok(_) => format!("Subagent {name} ({id}) has finished."),
        Err(failure) => format!(
            "Subagent {name} ({id}) has finished with an error: {}.",
            failure.reason()
        ),
    }
}

impl Steering for Registry {
    fn drain(&self) -> Vec<String> {
        // The observed lines stay a pure query, an unfetched report must
        // not be lost; the injected ones are delivered once, because
        // there is nothing to fetch behind them.
        let mut inner = self.lock();
        let mut lines = notification_lines(&inner);
        lines.append(&mut inner.extra);
        lines
    }
}

/// The notification lines of the jobs the registry observed, oldest
/// first, a pure query.
fn notification_lines(inner: &RegistryInner) -> Vec<String> {
    let mut jobs: Vec<(&str, &Job)> = inner
        .jobs
        .iter()
        .filter(|(_, job)| {
            matches!(
                job.state,
                State::Finished {
                    fetched: false,
                    announced: false,
                    ..
                }
            )
        })
        .map(|(id, job)| (id.as_str(), job))
        .collect();
    jobs.sort_by_key(|(_, job)| job.seq);
    jobs.iter()
        .map(|(id, job)| match &job.state {
            State::Finished { outcome, .. } => announcement(&job.name, id, outcome),
            _ => unreachable!("filtered to finished jobs"),
        })
        .collect()
}

/// Waits for a concurrency slot while the owning registry stays alive.
/// Returns `None` when the registry dropped first: the job's agent is
/// gone and the queued job never starts.
pub(crate) async fn acquire_slot(
    registry: &std::sync::Weak<Registry>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    loop {
        // Take the strong ref only for the check: a queued job never
        // keeps a dropped registry alive.
        let permit = registry.upgrade().and_then(|reg| reg.try_slot());
        if permit.is_some() {
            return permit;
        }
        registry.upgrade()?;
        tokio::time::sleep(SLOT_POLL).await;
    }
}

#[cfg(test)]
mod tests;
