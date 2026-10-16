//! The task table: user-scheduled prompts in SQLite, the same database
//! file the dispatcher's [`SqliteStore`](rhema_rsd::SqliteStore), the
//! watch table, and the notification table use.
//!
//! A task is the spec §1-compliant "cron": the row is inert data until
//! the [`TaskScheduler`](crate::task_source::TaskScheduler) — a
//! deterministic source, never the model — reads `next_due_millis`
//! against the clock and ingests the prompt as one event. No timer
//! invokes the model; a due task is an event, not a heartbeat.
//!
//! Two connections, one file: this store opens its own rusqlite
//! connection to the same database as the dispatcher's store — WAL and a
//! busy timeout, the pattern [`ClawStore`](crate::watch_store::ClawStore)
//! established — and the two interleave through SQLite's own locking.
//! Writes are single INSERT / UPDATE / DELETE statements, so there is no
//! read-modify-write window to guard.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::params;
use serde::Serialize;

/// The task table. The CHECK pins the two kinds' column pairing to what
/// the code writes: `at` rows carry `at_millis`, `every` rows carry
/// `interval_millis`. `deliver_to` names the session a firing wakes —
/// `NULL` meaning the task's own `task:<id>` slot.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id              INTEGER PRIMARY KEY,
    prompt          TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK(kind IN ('at','every')),
    at_millis       INTEGER NULL,
    interval_millis INTEGER NULL,
    next_due_millis INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    deliver_to      TEXT NULL
);
";

/// Adds `deliver_to` to a table created before it existed. The column
/// is the only schema the first release lacked, and `ALTER TABLE ADD
/// COLUMN` with a `NULL` default is the one shape SQLite can add
/// in place.
fn migrate_add_deliver_to(conn: &rusqlite::Connection) -> Result<(), TaskStoreError> {
    let present: bool = conn
        .prepare("PRAGMA table_info(tasks)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == "deliver_to");
    if !present {
        conn.execute("ALTER TABLE tasks ADD COLUMN deliver_to TEXT NULL", [])?;
    }
    Ok(())
}

/// Why a task-table operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TaskStoreError {
    /// The SQLite backend failed.
    #[error("task store backend: {0}")]
    Backend(#[from] rusqlite::Error),
}

/// What kind of task a row is: fire once at a time (`at`) or repeat on
/// an interval (`every`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    /// Fire once, at `at_millis`; the row leaves the table after
    /// firing.
    At,
    /// Fire on every `interval_millis` tick; the row re-arms after each
    /// firing.
    Every,
}

impl TaskKind {
    /// The name the table's `kind` column stores.
    pub fn name(self) -> &'static str {
        match self {
            TaskKind::At => "at",
            TaskKind::Every => "every",
        }
    }

    /// Parses the column spelling. Private: rows only enter through
    /// [`TaskStore::schedule_at`] / [`TaskStore::schedule_every`], and
    /// this guards only hand-edited tables.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "at" => Some(TaskKind::At),
            "every" => Some(TaskKind::Every),
            _ => None,
        }
    }
}

/// One scheduled prompt, as stored. Serializes whole: this is the shape
/// `list_tasks` serves the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskRecord {
    /// The row id; the dispatch key (`task:<id>`) and `cancel_task`
    /// address it.
    pub id: i64,
    /// The prompt the scheduled turn runs.
    pub prompt: String,
    /// Fire once or repeat.
    pub kind: TaskKind,
    /// The one-shot's firing time, milliseconds since the Unix epoch.
    pub at_millis: Option<i64>,
    /// The recurring task's interval, in milliseconds.
    pub interval_millis: Option<i64>,
    /// When the task next fires, milliseconds since the Unix epoch.
    pub next_due_millis: i64,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
    /// The session a firing wakes; `None` meaning the task's own
    /// `task:<id>` slot.
    pub deliver_to: Option<String>,
}

/// The persistent task table over its own connection to the daemon's
/// SQLite file. Shareable behind an `Arc`; every operation takes
/// `&self`.
pub struct TaskStore {
    conn: Mutex<rusqlite::Connection>,
}

impl TaskStore {
    /// Opens (creating if needed) the task table in the database at
    /// `path` — the same file the dispatcher's store holds. WAL keeps
    /// the connections from blocking each other's readers; the busy
    /// timeout rides out the other's write locks.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(TaskStoreError::from)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory table: the shape without the file, for
    /// tests that need no sharing.
    pub fn in_memory() -> Result<Self, TaskStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(TaskStoreError::from)?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, TaskStoreError> {
        if durable {
            // Same pragmas as the dispatcher's store on the same file:
            // SQLite stores the journal mode per database, and both
            // connections must agree that it is WAL.
            let _mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(TaskStoreError::from)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(TaskStoreError::from)?;
        }
        conn.execute_batch(SCHEMA).map_err(TaskStoreError::from)?;
        migrate_add_deliver_to(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Schedules a one-shot firing at `at_millis`, waking `deliver_to`
    /// (the task's own `task:<id>` slot when `None`). A time already
    /// past is accepted: the next detect pass fires it immediately — a
    /// task scheduled for "15:00" at 15:01 still runs.
    pub fn schedule_at(
        &self,
        prompt: &str,
        at_millis: i64,
        now: i64,
        deliver_to: Option<&str>,
    ) -> Result<TaskRecord, TaskStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO tasks (prompt, kind, at_millis, interval_millis, next_due_millis, created_at, deliver_to)
             VALUES (?1, 'at', ?2, NULL, ?2, ?3, ?4)",
            params![prompt, at_millis, now, deliver_to],
        )?;
        Ok(TaskRecord {
            id: conn.last_insert_rowid(),
            prompt: prompt.to_owned(),
            kind: TaskKind::At,
            at_millis: Some(at_millis),
            interval_millis: None,
            next_due_millis: at_millis,
            created_at: now,
            deliver_to: deliver_to.map(str::to_owned),
        })
    }

    /// Schedules a recurring firing every `interval_millis`, the first
    /// one `interval_millis` from `now`, waking `deliver_to`.
    pub fn schedule_every(
        &self,
        prompt: &str,
        interval_millis: i64,
        now: i64,
        deliver_to: Option<&str>,
    ) -> Result<TaskRecord, TaskStoreError> {
        let next_due = now + interval_millis;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO tasks (prompt, kind, at_millis, interval_millis, next_due_millis, created_at, deliver_to)
             VALUES (?1, 'every', NULL, ?2, ?3, ?4, ?5)",
            params![prompt, interval_millis, next_due, now, deliver_to],
        )?;
        Ok(TaskRecord {
            id: conn.last_insert_rowid(),
            prompt: prompt.to_owned(),
            kind: TaskKind::Every,
            at_millis: None,
            interval_millis: Some(interval_millis),
            next_due_millis: next_due,
            created_at: now,
            deliver_to: deliver_to.map(str::to_owned),
        })
    }

    /// Every task, oldest first — the `list_tasks` view.
    pub fn list(&self) -> Result<Vec<TaskRecord>, TaskStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, prompt, kind, at_millis, interval_millis, next_due_millis, created_at, deliver_to
                 FROM tasks ORDER BY id ASC",
            )
            .map_err(TaskStoreError::from)?;
        let rows = stmt
            .query_map([], row_task)
            .map_err(TaskStoreError::from)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(TaskStoreError::from)?;
        Ok(rows)
    }

    /// The due tasks at `now`, in firing order: `next_due_millis`
    /// elapsed, oldest due first. One detect pass reads this once and
    /// advances every row it acts on, so a task fires at most once per
    /// pass.
    pub fn due(&self, now: i64) -> Result<Vec<TaskRecord>, TaskStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare(
                "SELECT id, prompt, kind, at_millis, interval_millis, next_due_millis, created_at, deliver_to
                 FROM tasks WHERE next_due_millis <= ?1
                 ORDER BY next_due_millis ASC, id ASC",
            )
            .map_err(TaskStoreError::from)?;
        let rows = stmt
            .query_map(params![now], row_task)
            .map_err(TaskStoreError::from)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(TaskStoreError::from)?;
        Ok(rows)
    }

    /// Removes a task. `false` when there was none — the one-shot
    /// completion path and `cancel_task` share it, and both treat "already
    /// gone" as success.
    pub fn remove(&self, id: i64) -> Result<bool, TaskStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(removed == 1)
    }

    /// Moves a recurring task's next firing to `next_due_millis`. The
    /// scheduler computes the value — never `now + interval` inside a
    /// loop — so a task missed while the process was down catches up
    /// with one firing, not one per elapsed interval.
    pub fn rearm(&self, id: i64, next_due_millis: i64) -> Result<(), TaskStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "UPDATE tasks SET next_due_millis = ?2 WHERE id = ?1",
            params![id, next_due_millis],
        )?;
        Ok(())
    }
}

/// Maps one row; the shared tail of [`TaskStore::list`] and
/// [`TaskStore::due`].
fn row_task(row: &rusqlite::Row<'_>) -> Result<TaskRecord, rusqlite::Error> {
    let kind: String = row.get(2)?;
    Ok(TaskRecord {
        id: row.get(0)?,
        prompt: row.get(1)?,
        kind: TaskKind::parse(&kind)
            .unwrap_or_else(|| unreachable!("the CHECK constraint only admits 'at' and 'every'")),
        at_millis: row.get(3)?,
        interval_millis: row.get(4)?,
        next_due_millis: row.get(5)?,
        created_at: row.get(6)?,
        deliver_to: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_round_trip_through_the_table() {
        let store = TaskStore::in_memory().expect("store opens");
        let at = store
            .schedule_at("take the bread out", 1_500, 1_000, None)
            .expect("at lands");
        let every = store
            .schedule_every("stand up", 1_800_000, 1_000, None)
            .expect("every lands");
        assert_ne!(at.id, every.id);
        assert_eq!(at.deliver_to, None, "the default slot is the task's own");

        let listed = store.list().expect("list reads");
        assert_eq!(listed, vec![at.clone(), every.clone()]);
        assert_eq!(at.kind, TaskKind::At);
        assert_eq!(at.at_millis, Some(1_500));
        assert_eq!(at.next_due_millis, 1_500, "a one-shot is due at its at");
        assert_eq!(every.kind, TaskKind::Every);
        assert_eq!(every.interval_millis, Some(1_800_000));
        assert_eq!(every.next_due_millis, 1_801_000, "now + interval");
    }

    #[test]
    fn due_selects_only_elapsed_tasks_in_firing_order() {
        let store = TaskStore::in_memory().expect("store opens");
        let late = store.schedule_at("late", 500, 0, None).expect("at lands");
        let edge = store.schedule_at("edge", 1_000, 0, None).expect("at lands");
        store
            .schedule_at("future", 5_000, 0, None)
            .expect("at lands");

        assert!(
            store.due(499).expect("due reads").is_empty(),
            "nothing is due before the earliest"
        );
        assert_eq!(
            store
                .due(1_000)
                .expect("due reads")
                .iter()
                .map(|task| task.id)
                .collect::<Vec<_>>(),
            [late.id, edge.id],
            "the boundary is inclusive, in next-due order"
        );
    }

    #[test]
    fn remove_and_rearm_advance_rows() {
        let store = TaskStore::in_memory().expect("store opens");
        let task = store.schedule_every("tick", 1_000, 0, None).expect("lands");
        store.rearm(task.id, 9_999).expect("rearm lands");
        let listed = store.list().expect("list reads");
        assert_eq!(listed[0].next_due_millis, 9_999);
        assert!(store.remove(task.id).expect("remove lands"));
        assert!(!store.remove(task.id).expect("re-remove is quiet"));
        assert!(store.list().expect("list reads").is_empty());
    }

    #[test]
    fn a_deliver_to_session_round_trips_and_defaults_absent() {
        let store = TaskStore::in_memory().expect("store opens");
        store
            .schedule_at(
                "summarize tonight's work",
                1_500,
                1_000,
                Some("interactive:web"),
            )
            .expect("at lands");
        let listed = store.list().expect("list reads");
        assert_eq!(listed[0].deliver_to.as_deref(), Some("interactive:web"));
    }

    /// A table from before `deliver_to` opens into the new shape; its
    /// rows read as defaulting to the task's own slot.
    #[test]
    fn a_pre_deliver_to_table_migrates_at_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("opens");
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id              INTEGER PRIMARY KEY,
                     prompt          TEXT NOT NULL,
                     kind            TEXT NOT NULL CHECK(kind IN ('at','every')),
                     at_millis       INTEGER NULL,
                     interval_millis INTEGER NULL,
                     next_due_millis INTEGER NOT NULL,
                     created_at      INTEGER NOT NULL
                 );
                 INSERT INTO tasks (prompt, kind, at_millis, next_due_millis, created_at)
                     VALUES ('old row', 'at', 42, 42, 0);",
            )
            .expect("legacy table");
        }
        let store = TaskStore::open(&path).expect("opens and migrates");
        let listed = store.list().expect("list reads");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].deliver_to, None);
        // The migrated row still schedules with a delivery target.
        store
            .schedule_at("new row", 50, 40, Some("task:9"))
            .expect("schedules");
        assert_eq!(store.list().expect("reads").len(), 2);
    }

    #[test]
    fn two_connections_share_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        let writer = TaskStore::open(&path).expect("first connection");
        let reader = TaskStore::open(&path).expect("second connection");
        writer.schedule_at("hello", 42, 0, None).expect("schedule");
        let listed = reader.list().expect("the other connection sees it");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prompt, "hello");
    }
}
