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
//! Two read-only surfaces leave the registry: [`Registry::status`],
//! the subscription the UI's redraw tick renders one live line per
//! job from, and the [`Steering`] implementation, one notification
//! line per finished-but-unfetched job. Both are queries, never
//! mutations of record: the driver deduplicates announcements within a
//! turn, and a crash between its append and its checkpoint only
//! re-delivers.

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
    /// `queued`, the phase the inner turn last reported, or `finished`
    /// for a report the model has not fetched yet.
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
    /// the registry drops; `fetched` retires the job's UI line and its
    /// notifications.
    Finished {
        outcome: Result<String, String>,
        fetched: bool,
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
}

/// The jobs of one agent: ids, states, the report cache, the cap.
pub struct Registry {
    inner: Mutex<RegistryInner>,
    permits: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug)]
struct RegistryInner {
    next: u64,
    jobs: BTreeMap<String, Job>,
}

impl Registry {
    /// A registry running at most `max_running` top-level jobs at
    /// once; starts past it queue.
    pub fn new(max_running: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next: 0,
                jobs: BTreeMap::new(),
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
            },
        );
        Minted { id, cancel }
    }

    /// The job's task took a slot: the inner turn starts working.
    pub(crate) fn mark_running(&self, id: &str, model: Option<String>) {
        if let Some(job) = self.lock().jobs.get_mut(id) {
            if matches!(job.state, State::Queued) {
                job.state = State::Running {
                    phase: "thinking".to_owned(),
                    context: None,
                    spent: None,
                    model,
                };
            }
        }
    }

    /// Folds one [`rhema_core::ToolStatus`] from the job's inner turn
    /// into its record. Only a running job changes: a finished one
    /// keeps its last state.
    pub(crate) fn report(&self, id: &str, status: &rhema_core::ToolStatus) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(id) {
            if let State::Running {
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
    }

    /// The job's run ended: its report or the reason it failed. The
    /// report was capped at production; the cache is what was cut.
    pub(crate) fn finish(&self, id: &str, outcome: Result<String, String>) {
        if let Some(job) = self.lock().jobs.get_mut(id) {
            if !matches!(job.state, State::Finished { .. }) {
                job.state = State::Finished {
                    outcome,
                    fetched: false,
                };
            }
        }
    }

    /// Reads a job's report back — never an execution. A Report or
    /// Failed fetch marks the job fetched: its UI line leaves and it
    /// stops announcing, while the cache stays until the registry
    /// drops.
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
                Err(reason) => Fetched::Failed {
                    name: job.name.clone(),
                    reason: reason.clone(),
                },
            },
        };
        // The fetch retires the job: its UI line leaves, its
        // announcements stop. A Busy fetch retires nothing.
        if !matches!(fetched_state, Fetched::Busy { .. }) {
            if let State::Finished { fetched, .. } = &mut job.state {
                *fetched = true;
            }
        }
        fetched_state
    }

    /// The notification lines: one per finished-but-unfetched job,
    /// oldest first. A pure query — calling it twice returns the same
    /// lines; the driver deduplicates within a turn.
    pub(crate) fn notifications(&self) -> Vec<String> {
        let inner = self.lock();
        let mut jobs: Vec<(&str, &Job)> = inner
            .jobs
            .iter()
            .filter(|(_, job)| matches!(job.state, State::Finished { fetched: false, .. }))
            .map(|(id, job)| (id.as_str(), job))
            .collect();
        jobs.sort_by_key(|(_, job)| job.seq);
        jobs.iter()
            .map(|(id, job)| match &job.state {
                State::Finished { outcome: Ok(_), .. } => {
                    format!("Subagent {} ({id}) has finished.", job.name)
                }
                State::Finished {
                    outcome: Err(reason),
                    ..
                } => format!(
                    "Subagent {} ({id}) has finished with an error: {reason}.",
                    job.name
                ),
                _ => unreachable!("filtered to finished jobs"),
            })
            .collect()
    }

    /// The subscription the UI renders: one entry per queued, running,
    /// or finished-but-unfetched job. A fetched job leaves the snapshot
    /// — the transcript's tool row carries the outcome now.
    pub fn status(&self) -> Vec<JobStatus> {
        let inner = self.lock();
        let mut jobs: Vec<(&str, &Job)> = inner
            .jobs
            .iter()
            .filter(|(_, job)| !matches!(job.state, State::Finished { fetched: true, .. }))
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
                    State::Finished { .. } => ("finished".to_owned(), None, None, None),
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
        // Finished-but-unfetched: still on the snapshot, still
        // announcing.
        assert_eq!(registry.status()[0].phase, "finished");
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
    fn a_failed_job_fetches_as_its_reason() {
        let registry = registry();
        let job = registry.mint("reviewer");
        registry.finish(&job.id, Err("hit its step budget of 256".to_owned()));
        // The failure announced itself before the fetch.
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
        assert_eq!(
            registry.status()[0].phase,
            "finished",
            "a status after the finish changes nothing"
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
        registry.finish(&second.id, Ok("r2".to_owned()));
        registry.finish(&first.id, Ok("r1".to_owned()));
        let ids: Vec<String> = registry.status().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, vec!["job-1", "job-2"], "mint order, not id order");
        assert_eq!(
            registry.notifications()[0],
            "Subagent alpha (job-1) has finished."
        );
    }

    #[test]
    fn dropping_the_registry_flips_every_cancel_flag() {
        let registry = registry();
        let job = registry.mint("scout");
        let cancel = job.cancel.clone();
        drop(registry);
        assert!(cancel.load(Ordering::Relaxed), "the job stops");
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
