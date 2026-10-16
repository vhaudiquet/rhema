//! The task source: deterministic detection of due scheduled tasks, the
//! spec §1-compliant cron.
//!
//! The [`TaskScheduler`] is a [`Source`] named `"tasks"`: its `detect`
//! pass reads the task table against an injected
//! [`Clock`](rhema_rsd::Clock), ingests one event per due task into the
//! resident dispatcher, then advances the row — the one-shot is
//! deleted, the recurring task re-arms. The model is never reached here;
//! the turn happens because the dispatcher admitted the key, exactly as
//! for a chat message. Cadence belongs to the daemon's loop, which runs
//! this source's detect each tick alongside the forge sources.
//!
//! Due/re-arm semantics (the loop-fire guards):
//!
//! - A row fires at most once per detect pass: every acted-on row is
//!   advanced before the pass returns, and `due` reads the table once.
//! - A recurring task re-arms to `next_due + interval`; when that is
//!   still past (the process was down, the clock jumped), it re-arms to
//!   `now + interval` — a missed task catches up with one firing, never
//!   one per elapsed interval.
//! - A recurring row without a usable interval is deleted, not skipped:
//!   a skipped due row would fire again on every pass forever.

use std::sync::Arc;

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_rsd::{Clock, Dispatcher, Event, KeyFn};
use serde_json::json;

use crate::interactive::passthrough_key_fn;
use crate::task_store::{TaskKind, TaskStore};

/// The source name the scheduler ingests under; registered on the
/// resident dispatcher, so allowlists and `/v1/ingest` can name it.
pub const TASKS_SOURCE: &str = "tasks";

/// One scheduled task's dispatch key: `task:<id>` — the session the
/// prompt's turn runs in, persistent across the task's firings.
pub fn task_key(id: i64) -> String {
    format!("task:{id}")
}

/// The event one due firing becomes. The key is the task's delivery
/// target — its own `task:<id>` slot, or the session the scheduler was
/// asked to wake (a chat conversation with its context, a phone key
/// whose reply auto-notifies). The dedup id carries the firing's
/// `next_due`, so a recurring task's next firing never dedups against
/// the last one still pending in the key's inbox.
pub fn task_event(task: &crate::task_store::TaskRecord, now: u64) -> Event {
    let default_key = task_key(task.id);
    let key = task
        .deliver_to
        .as_deref()
        .filter(|key| !key.is_empty())
        .unwrap_or(default_key.as_str());
    Event {
        id: format!("task-{}@{}", task.id, task.next_due_millis),
        source: TASKS_SOURCE.to_owned(),
        payload: json!({ "key": key, "prompt": task.prompt }),
        received_at: now,
    }
}

/// The detectable stream of due tasks: the task table against a clock,
/// feeding the resident dispatcher.
pub struct TaskScheduler {
    clock: Arc<dyn Clock>,
    store: Arc<TaskStore>,
    dispatcher: Arc<Dispatcher>,
    key_fn: Arc<KeyFn>,
}

impl TaskScheduler {
    /// Registers the `tasks` source's key function on `dispatcher` and
    /// returns the scheduler feeding it — the ForgeSource shape: one
    /// source, registered once, before the first detect.
    pub fn new(dispatcher: &Arc<Dispatcher>, clock: Arc<dyn Clock>, store: Arc<TaskStore>) -> Self {
        let key_fn = passthrough_key_fn();
        dispatcher.register_source(TASKS_SOURCE, key_fn.clone());
        Self {
            clock,
            store,
            dispatcher: dispatcher.clone(),
            key_fn,
        }
    }
}

#[async_trait]
impl Source for TaskScheduler {
    fn name(&self) -> &str {
        TASKS_SOURCE
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        self.key_fn.clone()
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let now = self.clock.now_millis() as i64;
        let due = self
            .store
            .due(now)
            .map_err(|err| SourceError::Backend(format!("task store: {err}")))?;
        let mut fed = 0;
        for task in due {
            self.dispatcher.ingest(task_event(&task, now as u64))?;
            fed += 1;
            match task.kind {
                TaskKind::At => {
                    // Fired once; the row's work is done.
                    self.store
                        .remove(task.id)
                        .map_err(|err| SourceError::Backend(format!("task store: {err}")))?;
                }
                TaskKind::Every => match task.interval_millis {
                    Some(interval) if interval > 0 => {
                        let mut next = task.next_due_millis.saturating_add(interval);
                        if next <= now {
                            // Catch-up: the scheduled next is already past
                            // (downtime, a clock jump). One firing settles
                            // the debt; the rest never happens.
                            next = now.saturating_add(interval);
                        }
                        self.store
                            .rearm(task.id, next)
                            .map_err(|err| SourceError::Backend(format!("task store: {err}")))?;
                    }
                    // A recurring task that cannot re-arm must not sit due
                    // forever, re-firing every pass: retire it, loudly.
                    interval => {
                        eprintln!(
                            "[claw] warning: task {} has no usable interval \
                             ({interval:?}); removing it",
                            task.id
                        );
                        self.store
                            .remove(task.id)
                            .map_err(|err| SourceError::Backend(format!("task store: {err}")))?;
                    }
                },
            }
        }
        Ok(fed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_rsd::{DispatcherConfig, FixedClock, SystemClock, VolatileStore};

    fn dispatcher() -> Arc<Dispatcher> {
        Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(SystemClock),
            Arc::new(VolatileStore::new()),
        ))
    }

    /// The dispatcher status of one task's key: how many events wait on
    /// it. A key never ingested reports the missing view — depth 0.
    fn inbox_depth(dispatcher: &Dispatcher, id: i64) -> usize {
        dispatcher
            .status(&rhema_rsd::ResourceKey::from(task_key(id)))
            .expect("status reads")
            .inbox_depth
    }

    #[tokio::test]
    async fn a_due_one_shot_fires_once_and_is_removed() {
        let clock = Arc::new(FixedClock::new(1_000));
        let store = Arc::new(TaskStore::in_memory().expect("store opens"));
        let dispatcher = dispatcher();
        let source = TaskScheduler::new(&dispatcher, clock.clone(), store.clone());
        store
            .schedule_at("remind the user: bread", 1_500, 1_000, None)
            .expect("scheduled");

        // Not yet due: skipped, nothing fed, no record.
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher, 1), 0, "nothing fired");

        // Due: exactly one event, on the task's key.
        clock.advance(500);
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        assert_eq!(inbox_depth(&dispatcher, 1), 1);

        // The row is gone; a second pass re-fires nothing.
        assert!(store.list().expect("list reads").is_empty());
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher, 1), 1, "the inbox keeps it");
    }

    #[tokio::test]
    async fn a_recurring_task_rearms_and_never_loop_fires() {
        let clock = Arc::new(FixedClock::new(1_000));
        let store = Arc::new(TaskStore::in_memory().expect("store opens"));
        let dispatcher = dispatcher();
        let source = TaskScheduler::new(&dispatcher, clock.clone(), store.clone());
        store
            .schedule_every("stand up", 1_000, 1_000, None)
            .expect("scheduled");

        // Due at 2000: one firing, re-armed to due + interval.
        clock.advance(1_500); // now = 2500
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        let task = &store.list().expect("list reads")[0];
        assert_eq!(task.next_due_millis, 2_000 + 1_000, "next_due + interval");
        assert_eq!(inbox_depth(&dispatcher, 1), 1);

        // Still past due after re-arm (the firing lagged): the next pass
        // fires again — but one detect never fires the same task twice,
        // and the catch-up rule re-arms from now, not from the backlog.
        clock.advance(10_000); // now = 12500
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        let task = &store.list().expect("list reads")[0];
        assert_eq!(
            task.next_due_millis, 13_500,
            "catch-up re-arms to now + interval, never loop-fires"
        );
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(
            inbox_depth(&dispatcher, 1),
            2,
            "one firing per pass: two passes, two events"
        );
    }

    #[tokio::test]
    async fn a_delivery_target_routes_the_firing_to_that_session() {
        let clock = Arc::new(FixedClock::new(1_000));
        let store = Arc::new(TaskStore::in_memory().expect("store opens"));
        let dispatcher = dispatcher();
        let source = TaskScheduler::new(&dispatcher, clock.clone(), store.clone());
        store
            .schedule_at(
                "summarize this evening's work",
                1_500,
                1_000,
                Some("interactive:web"),
            )
            .expect("scheduled");

        clock.advance(500);
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        // The firing woke the named session, not the task's own slot.
        let depth = |key: &str| {
            dispatcher
                .status(&rhema_rsd::ResourceKey::from(key))
                .expect("status reads")
                .inbox_depth
        };
        assert_eq!(depth("interactive:web"), 1);
        assert_eq!(depth(&task_key(1)), 0, "the default slot stays cold");
    }

    #[tokio::test]
    async fn an_unusable_recurring_row_is_retired_not_loop_fired() {
        let clock = Arc::new(FixedClock::new(1_000));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        // The table exists before the hand edit, so the row is the only
        // thing in it.
        drop(TaskStore::open(&path).expect("store opens"));
        {
            // Simulate the hand-edited table the store's API cannot
            // produce: an `every` row without an interval, due now. The
            // CHECK constraint only pins the kind's spelling.
            let conn = rusqlite::Connection::open(&path).expect("raw connection");
            conn.execute(
                "INSERT INTO tasks (prompt, kind, at_millis, interval_millis, next_due_millis, created_at)
                 VALUES ('tick', 'every', NULL, NULL, 0, 0)",
                [],
            )
            .expect("the malformed row lands");
        }
        let store = Arc::new(TaskStore::open(&path).expect("store reopens"));
        let dispatcher = dispatcher();
        let source = TaskScheduler::new(&dispatcher, clock.clone(), store.clone());

        // The bad row fires once (its prompt is real), then is retired:
        // a skip would leave it due on every pass forever.
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        assert!(store.list().expect("list reads").is_empty());
        assert_eq!(source.detect().await.expect("detect runs"), 0);
    }
}
