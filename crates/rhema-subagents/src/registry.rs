//! The job registry: everything a started subagent becomes.
//!
//! A start call mints a job id and registers it here
//! (`queued → running → finished(ok, report) | finished(error,
//! reason)`); the report is cached at production, capped, and a fetch
//! only reads it back — a double-fetch returns the cache, never a
//! re-run, because subagents are [`ReplaySafety::Unsafe`](rhema_core::ReplaySafety)
//! and the fetch is a read of this registry, not an execution. Each
//! job carries its own cancel flag — minted at spawn, never the parent
//! turn's — so escape stops the main session and running subagents do
//! not. Dropping the registry is the only stop switch: it flips every
//! job's flag, queued jobs never start, and running ones end at their
//! next step boundary — the jobs are bound to the agent's lifetime,
//! and their reports are lost with it.
//!
//! Concurrency: at most [`Registry::max_running`] top-level jobs run
//! at once; a start beyond the cap queues — the id is still minted and
//! returned at once, and the job begins when a slot frees.
//!
//! Three surfaces leave the registry. [`Registry::status`] is the
//! subscription the UI's redraw tick renders one working line per job
//! from — working only: a finished job leaves it at once, and the
//! finish moves to the transcript as a notice. The [`Steering`]
//! implementation is one notification line per finished-but-unfetched
//! job, a pure query — the driver deduplicates within a turn, and a
//! crash between its append and its checkpoint only re-delivers. And
//! the two consuming takes serve the frontend: [`Registry::take_finished`]
//! delivers each finish once, the transcript notice, and
//! [`Registry::take_notifications`] hands a session with no running
//! turn the pending lines to run as the next turn's prompt, retiring
//! them so the driver's drain does not repeat what the prompt already
//! said.
//!
//! Failures split into two classes, and the class decides what a
//! restart may do. A [`JobFailure::Transient`] names the
//! infrastructure — the model call died on the network or the server,
//! the same class the provider's retry ladder waits out — and the job
//! carried everything it needs to run again, so the user's retry
//! command restarts it in place: [`Registry::restart_failed`] moves
//! every transient-failed job back to queued under its own id, with a
//! fresh cancel flag and clock, and hands the spawn ingredients back
//! to the tool. The pending failure announcements retire with the
//! state they describe, the report cache clears, and the re-run
//! finishes into the same id — the model sees the failure, then a
//! finish, and fetches as it always did. A
//! [`JobFailure::Permanent`] names the work itself — no report, a
//! spent step budget — and never restarts: re-running it repeats the
//! failure at full cost.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rhema_coding_agent::Steering;
use rhema_core::Usage;

/// How many top-level jobs run at once when the constructor knob is
/// not set.
pub const DEFAULT_MAX_RUNNING: usize = 4;

/// How often a queued job polls for a freed slot. A job's start is not
/// latency-critical — its report is announced when it finishes — and a
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
    /// No job by that id — one the model invented, or one a restart
    /// lost (the registry is in-memory).
    Unknown,
}

/// Why a job ended without a report, and whether a retry could clear
/// it. The class is decided where the failure is known — the inner
/// turn's step error — and rides the finish, so a restart never has to
/// guess from the reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFailure {
    /// The infrastructure failed: a dead link, a transport timeout, a
    /// struggling server — every [`ProviderError`] a retry ladder
    /// would wait out. The job may run again; the user's retry
    /// command restarts it from its start.
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

    /// Whether the failure is one [`Registry::restart_failed`] picks
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
    pub(crate) hook: Option<std::sync::Arc<dyn rhema_agent_tools::Approval>>,
    /// The call's raw `model` argument, the same spelling the call
    /// carried; a restart re-resolves it against the factory, so a
    /// re-run lands where the original was aimed, not where a
    /// wire-time snapshot put it. `None` inherits the definition's
    /// frontmatter, then the parent's model.
    pub(crate) model: Option<String>,
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
    /// the registry drops. `fetched` retires the announcements (the
    /// cache stays); `announced` retires the notification line for a
    /// frontend that took it to run as a prompt.
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
    /// The mint order: status lines and notifications come out oldest
    /// first, not lexicographically by id.
    seq: u64,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
    state: State,
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

/// The most finished-but-untaken events one registry holds. The
/// frontend drains [`Registry::take_finished`] every frame; the cap
/// is the guard for a frontend that stops reading — each event names
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
    /// until its task takes a slot. The cancel flag is the job's own —
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
                seq,
                cancel: cancel.clone(),
                started_at: Instant::now(),
                state: State::Queued,
                rerun: None,
            },
        );
        Minted { id, cancel }
    }

    /// Arms a minted job for a later restart: the spawn ingredients a
    /// [`Self::restart_failed`] hands back. A start calls this right
    /// after minting; a job never armed (none today — every start
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
    /// keeps its last state.
    pub(crate) fn report(&self, id: &str, status: &rhema_core::ToolStatus) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(id)
            && let State::Running {
                phase,
                context,
                spent,
                model,
            } = &mut job.state
        {
            *phase = status.phase.clone();
            *context = status.context;
            *spent = status.spent;
            if status.model.is_some() {
                *model = status.model.clone();
            }
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

    /// Reads a job's report back — never an execution. A Report or
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
        // announcements stop. A Busy fetch retires nothing.
        if !matches!(fetched_state, Fetched::Busy { .. })
            && let State::Finished { fetched, .. } = &mut job.state
        {
            *fetched = true;
        }
        fetched_state
    }

    /// The notification lines: one per finished job neither fetched
    /// nor taken, oldest first. A pure query — calling it twice
    /// returns the same lines; the driver deduplicates within a turn,
    /// and a line a frontend took for its prompt is retired at the
    /// take, not here.
    pub(crate) fn notifications(&self) -> Vec<String> {
        let inner = self.lock();
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

    /// Takes the pending notification lines out, oldest first, marking
    /// each taken job announced: its line stops announcing here and in
    /// the [`Steering`] drain. The frontend's idle path — a session
    /// with no running turn runs the lines as the next turn's prompt,
    /// and this take keeps the driver's turn-start drain from
    /// repeating what the prompt already said. While a turn runs, the
    /// lines belong to the driver's steering: do not call this.
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
            }
        }
        taken.into_iter().map(|(_, _, line)| line).collect()
    }

    /// Takes the finish events out, oldest first — one per job,
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
                        "{} older subagent notice{} dropped — nobody was \
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
    /// refreshes the clock, and mints a fresh cancel flag — the old
    /// task is gone, so the re-run's task owns its own. Leaving
    /// `Finished` retires whatever was pending on it: the failure's
    /// notification line (it described a state that no longer exists,
    /// and a model that read it would delegate the same task beside
    /// the re-run), and the report cache (a fetch now reads "still
    /// running"). Finish events already in the feed stay — the failure
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
                Some(RestartJob {
                    id,
                    name: job.name.clone(),
                    rerun,
                    cancel,
                })
            })
            .collect()
    }

    /// The subscription the UI renders: one entry per working job —
    /// queued or running. A finished job leaves the snapshot at once
    /// (the finish notice carries the outcome) and its report stays on
    /// the fetch until the registry drops.
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

impl Drop for Registry {
    fn drop(&mut self) {
        // The agent is gone: running jobs stop at their next step
        // boundary, queued ones never start. Their reports are lost
        // with the registry — the session never carried them.
        for job in self.lock().jobs.values() {
            job.cancel.store(true, Ordering::Relaxed);
        }
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
        self.notifications()
    }
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
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::new(2)
    }

    /// Arms `job` with placeholder spawn ingredients, the way a start
    /// always does.
    fn arm(registry: &Registry, id: &str) {
        registry.arm_restart(
            id,
            JobRerun {
                def: crate::def::SubagentDef {
                    name: "scout".into(),
                    description: "explores".into(),
                    model: None,
                    max_steps: None,
                    body: "Explore.".into(),
                },
                prompt: "go".into(),
                parent_run: "turn-1".into(),
                model: None,
                hook: None,
            },
        );
    }

    #[test]
    fn ids_are_minted_in_order_and_unique() {
        let registry = registry();
        assert_eq!(registry.mint("a").id, "job-1");
        assert_eq!(registry.mint("b").id, "job-2");
    }

    #[test]
    fn a_fresh_job_reads_queued_and_an_unknown_id_is_unknown() {
        let registry = registry();
        let job = registry.mint("scout");
        assert_eq!(registry.status()[0].phase, "queued");
        assert_eq!(registry.status()[0].name, "scout");
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Busy {
                name: "scout".into()
            }
        );
        assert_eq!(registry.fetch("job-99"), Fetched::Unknown);
    }

    #[test]
    fn a_fetched_ok_job_serves_its_cached_report_and_leaves_the_snapshot() {
        let registry = registry();
        let job = registry.mint("scout");
        registry.finish(&job.id, Ok("the report".to_owned()));
        // Finished: off the working snapshot at once, the finish event
        // pending, the announcement still steering.
        assert!(registry.status().is_empty());
        assert_eq!(
            registry.take_finished(),
            vec![JobFinished {
                id: "job-1".into(),
                name: "scout".into(),
                line: "Subagent scout (job-1) has finished.".into(),
            }]
        );
        assert_eq!(
            registry.notifications(),
            vec!["Subagent scout (job-1) has finished.".to_owned()]
        );
        let fetched = registry.fetch(&job.id);
        assert_eq!(
            fetched,
            Fetched::Report {
                name: "scout".into(),
                report: "the report".into()
            }
        );
        // The cache survives the fetch; the announcement does not.
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Report {
                name: "scout".into(),
                report: "the report".into()
            }
        );
        assert!(registry.status().is_empty());
        assert!(registry.notifications().is_empty());
    }

    #[test]
    fn an_unread_feed_drops_the_oldest_and_says_so_once() {
        let registry = registry();
        for _ in 0..(MAX_FINISHED + 8) {
            let job = registry.mint("scout");
            registry.finish(&job.id, Ok("the report".to_owned()));
        }
        let events = registry.take_finished();
        assert!(
            events.len() <= MAX_FINISHED + 1,
            "the cap holds (plus the overflow marker): {}",
            events.len()
        );
        assert!(
            events[0].id.is_empty() && events[0].line.contains("dropped"),
            "the overflow speaks first: {:?}",
            events[0].line
        );
        // The marker speaks once: the next take is clean.
        assert!(registry.take_finished().is_empty());
        let job = registry.mint("scout");
        registry.finish(&job.id, Ok("the report".to_owned()));
        let events = registry.take_finished();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].id == job.id,
            "no stale marker: {:?}",
            events[0].line
        );
    }

    #[test]
    fn the_finish_feed_delivers_each_job_once_in_mint_order() {
        let registry = registry();
        let ok = registry.mint("scout");
        let bad = registry.mint("reviewer");
        registry.finish(
            &bad.id,
            Err(JobFailure::Permanent(
                "hit its step budget of 256".to_owned(),
            )),
        );
        registry.finish(&ok.id, Ok("the report".to_owned()));
        assert_eq!(
            registry.take_finished(),
            vec![
                JobFinished {
                    id: "job-1".into(),
                    name: "scout".into(),
                    line: "Subagent scout (job-1) has finished.".into(),
                },
                JobFinished {
                    id: "job-2".into(),
                    name: "reviewer".into(),
                    line: "Subagent reviewer (job-2) has finished with an error: hit its step budget of 256."
                        .into(),
                },
            ],
            "oldest first, whatever the finish order"
        );
        assert!(registry.take_finished().is_empty(), "one event per job");
    }

    #[test]
    fn a_taken_line_stops_announcing_but_a_finish_event_retires_nothing() {
        let registry = registry();
        let job = registry.mint("scout");
        registry.finish(&job.id, Ok("the report".to_owned()));
        // The notice feed alone leaves the steering line pending: the
        // driver still announces it.
        let _ = registry.take_finished();
        assert_eq!(
            registry.notifications(),
            vec!["Subagent scout (job-1) has finished.".to_owned()]
        );
        // The idle take retires the line: the prompt carries the text
        // now, and the driver's drain must not repeat it.
        assert_eq!(
            registry.take_notifications(),
            vec!["Subagent scout (job-1) has finished.".to_owned()]
        );
        assert!(registry.notifications().is_empty());
        assert!(registry.take_notifications().is_empty());
        // The report is still on the fetch.
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Report {
                name: "scout".into(),
                report: "the report".into()
            }
        );
    }

    #[test]
    fn the_idle_take_covers_only_finished_jobs() {
        let registry = registry();
        let done = registry.mint("alpha");
        let running = registry.mint("zeta");
        registry.finish(&done.id, Ok("r".to_owned()));
        registry.mark_running(&running.id, None);
        assert_eq!(
            registry.take_notifications(),
            vec!["Subagent alpha (job-1) has finished.".to_owned()],
            "a running job has no line to take"
        );
        // The running job keeps its snapshot line.
        let snapshot = registry.status();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, running.id);
        assert_eq!(snapshot[0].phase, "thinking");
    }

    #[test]
    fn a_failed_job_fetches_as_its_reason() {
        let registry = registry();
        let job = registry.mint("reviewer");
        registry.finish(
            &job.id,
            Err(JobFailure::Permanent(
                "hit its step budget of 256".to_owned(),
            )),
        );
        // The failure announced itself before the fetch, notice
        // included.
        assert_eq!(
            registry.take_finished()[0].line,
            "Subagent reviewer (job-1) has finished with an error: hit its step budget of 256."
        );
        assert_eq!(
            registry.notifications(),
            vec![
                "Subagent reviewer (job-1) has finished with an error: hit its step budget of 256."
                    .to_owned()
            ]
        );
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Failed {
                name: "reviewer".into(),
                reason: "hit its step budget of 256".into()
            }
        );
        assert!(
            registry.notifications().is_empty(),
            "fetched jobs stop announcing"
        );
    }

    #[test]
    fn a_late_report_never_overwrites_a_finish() {
        let registry = registry();
        let job = registry.mint("scout");
        registry.finish(&job.id, Ok("final".to_owned()));
        registry.report(
            &job.id,
            &rhema_core::ToolStatus {
                phase: "running".to_owned(),
                context: None,
                spent: None,
                model: Some("m".into()),
            },
        );
        // The finish announced itself; the late status did not silence it.
        assert!(
            !registry.notifications().is_empty(),
            "the announcement survives a report it must ignore"
        );
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Report {
                name: "scout".into(),
                report: "final".into()
            },
            "a late status changes nothing: the finish stands"
        );
        assert!(
            registry.notifications().is_empty(),
            "and the fetch retires the announcement, like any fetched job"
        );
    }

    #[test]
    fn a_running_job_reports_phase_context_and_spend() {
        let registry = registry();
        let job = registry.mint("scout");
        registry.mark_running(&job.id, Some("worker-model".into()));
        registry.report(
            &job.id,
            &rhema_core::ToolStatus {
                phase: "running".to_owned(),
                context: Some(rhema_core::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 90,
                }),
                spent: Some(rhema_core::Usage {
                    input_tokens: 200,
                    output_tokens: 20,
                    cached_input_tokens: 0,
                }),
                model: None,
            },
        );
        let status = &registry.status()[0];
        assert_eq!(status.phase, "running");
        assert_eq!(status.model.as_deref(), Some("worker-model"));
        assert_eq!(status.context.map(|u| u.input_tokens), Some(100));
        assert_eq!(status.spent.map(|u| u.output_tokens), Some(20));
    }

    #[test]
    fn status_and_notifications_come_out_oldest_first() {
        let registry = registry();
        let first = registry.mint("alpha");
        let second = registry.mint("zeta");
        let ids: Vec<String> = registry.status().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, vec!["job-1", "job-2"], "mint order, not id order");
        registry.finish(&second.id, Ok("r2".to_owned()));
        registry.finish(&first.id, Ok("r1".to_owned()));
        assert_eq!(
            registry.notifications()[0],
            "Subagent alpha (job-1) has finished."
        );
        let events = registry.take_finished();
        let names: Vec<&str> = events.iter().map(|event| event.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"], "mint order, not id order");
        assert!(registry.status().is_empty(), "finished jobs leave it");
    }

    #[test]
    fn dropping_the_registry_flips_every_cancel_flag() {
        let registry = registry();
        let job = registry.mint("scout");
        let cancel = job.cancel.clone();
        drop(registry);
        assert!(cancel.load(Ordering::Relaxed), "the job stops");
    }

    #[test]
    fn a_transient_failure_restarts_in_place_and_retires_the_announcements() {
        let registry = registry();
        let job = registry.mint("scout");
        arm(&registry, &job.id);
        registry.finish(
            &job.id,
            Err(JobFailure::Transient("network error: dead link".to_owned())),
        );
        assert_eq!(
            registry.notifications(),
            vec![
                "Subagent scout (job-1) has finished with an error: network error: dead link."
                    .to_owned()
            ]
        );

        let restarted = registry.restart_failed();
        assert_eq!(restarted.len(), 1);
        assert_eq!(restarted[0].id, job.id);
        assert_eq!(restarted[0].name, "scout");
        assert_eq!(restarted[0].rerun.prompt, "go");

        // Same id, back to queued, clock fresh, cancel flag fresh: the
        // re-run is the job's own continuation.
        let snapshot = registry.status();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, job.id);
        assert_eq!(snapshot[0].phase, "queued");
        assert_eq!(
            registry.fetch(&job.id),
            Fetched::Busy {
                name: "scout".into()
            },
            "the failed report's cache cleared with the state it described"
        );
        assert!(
            registry.notifications().is_empty(),
            "the stale failure never steers: the model would delegate beside the re-run"
        );
        assert!(
            !restarted[0].cancel.load(Ordering::Relaxed),
            "the re-run's flag mints fresh"
        );
    }

    #[test]
    fn a_restarted_job_can_fail_transiently_and_restart_again() {
        let registry = registry();
        let job = registry.mint("scout");
        arm(&registry, &job.id);
        registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
        assert_eq!(registry.restart_failed().len(), 1);
        // The re-run failed the same way: the ingredients survive the
        // restart, so the retry command keeps working.
        registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
        assert_eq!(registry.restart_failed().len(), 1);
    }

    #[test]
    fn only_transient_failures_restart() {
        let registry = registry();
        let transient = registry.mint("scout");
        arm(&registry, &transient.id);
        let permanent = registry.mint("reviewer");
        arm(&registry, &permanent.id);
        let finished = registry.mint("writer");
        arm(&registry, &finished.id);
        registry.finish(
            &transient.id,
            Err(JobFailure::Transient("network".to_owned())),
        );
        registry.finish(
            &permanent.id,
            Err(JobFailure::Permanent(
                "hit its step budget of 256".to_owned(),
            )),
        );
        registry.finish(&finished.id, Ok("the report".to_owned()));

        let restarted = registry.restart_failed();
        assert_eq!(
            restarted
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec![transient.id],
            "a spent budget and a finished report never re-run"
        );
    }

    #[test]
    fn a_job_never_armed_never_restarts() {
        // Unreachable through the tool (every start arms), but the
        // fail-safe is no restart, never a spawn without ingredients.
        let registry = registry();
        let job = registry.mint("scout");
        registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
        assert!(registry.restart_failed().is_empty());
    }

    #[test]
    fn restarts_come_out_in_mint_order() {
        let registry = registry();
        let first = registry.mint("alpha");
        let second = registry.mint("zeta");
        arm(&registry, &first.id);
        arm(&registry, &second.id);
        registry.finish(&second.id, Err(JobFailure::Transient("network".to_owned())));
        registry.finish(&first.id, Err(JobFailure::Transient("network".to_owned())));
        let restarted = registry.restart_failed();
        let ids: Vec<&str> = restarted.iter().map(|job| job.id.as_str()).collect();
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[tokio::test]
    async fn a_queued_job_never_starts_after_the_registry_drops() {
        // Cap 1, slot taken: the queued wait parks. Dropping the
        // registry ends the wait with None instead of a permit.
        let registry = Arc::new(Registry::new(1));
        let slot = registry.try_slot().expect("the one slot");
        registry.mint("holder");
        let weak = Arc::downgrade(&registry);
        let waiter = tokio::spawn(async move { acquire_slot(&weak).await });
        tokio::time::sleep(SLOT_POLL * 3).await;
        drop(registry);
        drop(slot);
        assert!(waiter.await.unwrap().is_none(), "no permit past the drop");
    }
}
