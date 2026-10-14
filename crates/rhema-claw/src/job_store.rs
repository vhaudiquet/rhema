//! The coding-job store: the instances the resident may drive a coding
//! agent in, and the jobs queued against them.
//!
//! An **instance** is a named workspace: the audit unit for "which
//! checkouts can the model drive a coding agent in". A **job** is one
//! `CodingAgent` turn over an instance's workspace: prompt in, final
//! text out, run off the chat turn by the jobs dispatcher (see
//! [`crate::jobs`]). The conversation carries across a job boundary in
//! the instance row's session blob — the same stash the forge handler
//! keeps in the dispatcher's `driver_state`, keyed by instance instead
//! of by thread, so "run the tests" after "fix the lint" continues the
//! same coding session.
//!
//! Two invariants the schema itself holds: at most one **live** job
//! (queued or running) per instance — a partial unique index refuses
//! the second enqueue, because two coding agents in one workspace is
//! two agents editing the same files — and [`CodingJobStore::recover`]
//! is the boot pass that makes a crash honest: a row left `running` by
//! a dead process is failed as interrupted, and a row still `queued`
//! gets its timestamp bumped so its re-ingest carries a fresh dedup id
//! (the old one may already sit in the dispatcher's consumed set).

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::params;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum JobStoreError {
    /// The SQLite layer failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The instance name is empty or carries a character the dispatch
    /// key cannot hold (`#`, `/`, `:`) or whitespace.
    #[error("invalid instance name {name:?}: use letters, digits, dash, dot, underscore")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
    /// An instance with this name already exists.
    #[error("instance {name:?} already exists")]
    Duplicate {
        /// The refused name.
        name: String,
    },
    /// The instance is not in the table.
    #[error("no instance named {name:?}")]
    UnknownInstance {
        /// The name that resolved to nothing.
        name: String,
    },
    /// The instance has a live job; the operation (a second enqueue, a
    /// drop) is refused until it finishes.
    #[error("instance {name:?} has a job in flight; check its status or wait")]
    Busy {
        /// The busy instance.
        name: String,
    },
}

/// How far a job has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Enqueued; the jobs source will ingest it.
    Queued,
    /// A coding-agent turn is running for it.
    Running,
    /// The turn's final text landed in `result`.
    Done,
    /// The turn failed; `result` carries the reason.
    Failed,
}

impl JobStatus {
    /// The spelling `status` answers carry.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "running" => JobStatus::Running,
            "done" => JobStatus::Done,
            "failed" => JobStatus::Failed,
            _ => JobStatus::Queued,
        }
    }

    /// Whether the job still has a turn ahead of it.
    pub fn is_live(self) -> bool {
        matches!(self, JobStatus::Queued | JobStatus::Running)
    }
}

/// One named workspace the resident may drive a coding agent in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    /// The name the tools address it by.
    pub name: String,
    /// The workspace directory the coding agent's shell runs in.
    pub path: String,
    /// The coding session the next job resumes, as JSON; `None` until
    /// the first job's turn returns.
    pub session: Option<String>,
    /// When the instance was added.
    pub created_at_millis: i64,
}

/// One queued or finished coding run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// The row id; the dispatch key and the `status` answer carry it.
    pub id: i64,
    /// The instance the job runs against.
    pub instance: String,
    /// The prompt the coding agent turns on.
    pub prompt: String,
    /// How far the job has got.
    pub status: JobStatus,
    /// The final text on done, the reason on failed.
    pub result: Option<String>,
    /// When the job was enqueued.
    pub created_at_millis: i64,
    /// The last transition; also the dedup id's half, so a re-queued
    /// row re-ingests after a restart.
    pub updated_at_millis: i64,
}

/// The instances and jobs, in one SQLite file beside the daemon's other
/// tables.
pub struct CodingJobStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl CodingJobStore {
    /// Opens the store at `path`, creating the tables.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JobStoreError> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init(conn)
    }

    /// A memory-backed store; the tests' shape.
    pub fn in_memory() -> Result<Self, JobStoreError> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, JobStoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS coding_instances (
                 name              TEXT PRIMARY KEY,
                 path              TEXT NOT NULL,
                 session           TEXT,
                 created_at_millis INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS coding_jobs (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 instance          TEXT NOT NULL,
                 prompt            TEXT NOT NULL,
                 status            TEXT NOT NULL,
                 result            TEXT,
                 created_at_millis INTEGER NOT NULL,
                 updated_at_millis INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS coding_jobs_one_live_per_instance
                 ON coding_jobs(instance) WHERE status IN ('queued', 'running');",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Adds an instance. The name is validated here — the dispatch key
    /// is `job:<name>#<id>`, so a `#` in the name would break the parse
    /// the jobs handler depends on.
    pub fn add_instance(&self, name: &str, path: &str) -> Result<InstanceRecord, JobStoreError> {
        if name.is_empty()
            || name
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '#' | '/' | ':'))
        {
            return Err(JobStoreError::InvalidName { name: name.into() });
        }
        let created_at = now_millis();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO coding_instances (name, path, session, created_at_millis)
             VALUES (?1, ?2, NULL, ?3)",
            params![name, path, created_at],
        )
        .map_err(|err| match err {
            rusqlite::Error::SqliteFailure(_, Some(text)) if text.contains("UNIQUE") => {
                JobStoreError::Duplicate { name: name.into() }
            }
            other => other.into(),
        })?;
        Ok(InstanceRecord {
            name: name.to_owned(),
            path: path.to_owned(),
            session: None,
            created_at_millis: created_at,
        })
    }

    /// Removes an instance. Refused while a job is live on it: a drop
    /// under a running turn would orphan the run's store writes.
    pub fn drop_instance(&self, name: &str) -> Result<bool, JobStoreError> {
        let conn = self.lock();
        if Self::live_locked(&conn, name)?.is_some() {
            return Err(JobStoreError::Busy { name: name.into() });
        }
        let dropped = conn.execute(
            "DELETE FROM coding_instances WHERE name = ?1",
            params![name],
        )?;
        Ok(dropped > 0)
    }

    /// Every instance, oldest first.
    pub fn instances(&self) -> Result<Vec<InstanceRecord>, JobStoreError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT name, path, session, created_at_millis
             FROM coding_instances ORDER BY created_at_millis, name",
        )?;
        let rows = statement.query_map([], row_instance)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(JobStoreError::from)
    }

    /// One instance by name.
    pub fn instance(&self, name: &str) -> Result<Option<InstanceRecord>, JobStoreError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT name, path, session, created_at_millis
             FROM coding_instances WHERE name = ?1",
        )?;
        let mut rows = statement.query_map(params![name], row_instance)?;
        rows.next().transpose().map_err(JobStoreError::from)
    }

    /// The instance's session blob, `None` until the first job's turn
    /// returns or after a `fresh` run cleared it.
    pub fn session(&self, name: &str) -> Result<Option<String>, JobStoreError> {
        self.instance(name)?
            .map(|instance| instance.session)
            .ok_or_else(|| JobStoreError::UnknownInstance { name: name.into() })
    }

    /// Replaces the instance's session blob — the coding conversation
    /// the next job resumes.
    pub fn save_session(&self, name: &str, session: &str) -> Result<(), JobStoreError> {
        let conn = self.lock();
        let updated = conn.execute(
            "UPDATE coding_instances SET session = ?2 WHERE name = ?1",
            params![name, session],
        )?;
        (updated > 0)
            .then_some(())
            .ok_or_else(|| JobStoreError::UnknownInstance { name: name.into() })
    }

    /// Drops the session blob: the next `run` starts a fresh coding
    /// conversation.
    pub fn clear_session(&self, name: &str) -> Result<(), JobStoreError> {
        self.save_session_name(name, None)
    }

    fn save_session_name(&self, name: &str, session: Option<&str>) -> Result<(), JobStoreError> {
        let conn = self.lock();
        let updated = conn.execute(
            "UPDATE coding_instances SET session = ?2 WHERE name = ?1",
            params![name, session],
        )?;
        (updated > 0)
            .then_some(())
            .ok_or_else(|| JobStoreError::UnknownInstance { name: name.into() })
    }

    /// Enqueues one job. Refused when the instance is unknown or
    /// already has a live job.
    pub fn enqueue(&self, instance: &str, prompt: &str) -> Result<JobRecord, JobStoreError> {
        let conn = self.lock();
        if conn
            .query_row(
                "SELECT 1 FROM coding_instances WHERE name = ?1",
                params![instance],
                |_| Ok(()),
            )
            .is_err()
        {
            return Err(JobStoreError::UnknownInstance {
                name: instance.into(),
            });
        }
        let now = now_millis();
        conn.execute(
            "INSERT INTO coding_jobs (instance, prompt, status, created_at_millis, updated_at_millis)
             VALUES (?1, ?2, 'queued', ?3, ?3)",
            params![instance, prompt, now],
        )
        .map_err(|err| match err {
            rusqlite::Error::SqliteFailure(_, Some(text))
                if text.contains("UNIQUE") || text.contains("constraint") =>
            {
                JobStoreError::Busy { name: instance.into() }
            }
            other => other.into(),
        })?;
        let id = conn.last_insert_rowid();
        Ok(JobRecord {
            id,
            instance: instance.to_owned(),
            prompt: prompt.to_owned(),
            status: JobStatus::Queued,
            result: None,
            created_at_millis: now,
            updated_at_millis: now,
        })
    }

    /// One job by id.
    pub fn job(&self, id: i64) -> Result<Option<JobRecord>, JobStoreError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, instance, prompt, status, result, created_at_millis, updated_at_millis
             FROM coding_jobs WHERE id = ?1",
        )?;
        let mut rows = statement.query_map(params![id], row_job)?;
        rows.next().transpose().map_err(JobStoreError::from)
    }

    /// The instance's jobs, newest first, at most `limit`.
    pub fn jobs_for(&self, instance: &str, limit: u32) -> Result<Vec<JobRecord>, JobStoreError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, instance, prompt, status, result, created_at_millis, updated_at_millis
             FROM coding_jobs WHERE instance = ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![instance, limit], row_job)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(JobStoreError::from)
    }

    /// The queued rows, oldest first — the jobs source's detect pass.
    pub fn queued(&self) -> Result<Vec<JobRecord>, JobStoreError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, instance, prompt, status, result, created_at_millis, updated_at_millis
             FROM coding_jobs WHERE status = 'queued' ORDER BY id",
        )?;
        let rows = statement.query_map([], row_job)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(JobStoreError::from)
    }

    /// The instance's live job, if any — the busy check the tools and
    /// the drop path share.
    pub fn live_job(&self, instance: &str) -> Result<Option<JobRecord>, JobStoreError> {
        let conn = self.lock();
        Self::live_locked(&conn, instance)
    }

    /// Marks the job running; the turn has begun.
    pub fn mark_running(&self, id: i64) -> Result<(), JobStoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE coding_jobs SET status = 'running', updated_at_millis = ?2
             WHERE id = ?1",
            params![id, now_millis()],
        )?;
        Ok(())
    }

    /// Finishes the job with the turn's final text.
    pub fn finish(&self, id: i64, result: &str) -> Result<(), JobStoreError> {
        self.transition(id, "done", Some(result))
    }

    /// Fails the job with the reason; the model can read it via status.
    pub fn fail(&self, id: i64, reason: &str) -> Result<(), JobStoreError> {
        self.transition(id, "failed", Some(reason))
    }

    fn transition(&self, id: i64, status: &str, result: Option<&str>) -> Result<(), JobStoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE coding_jobs SET status = ?2, result = ?3, updated_at_millis = ?4
             WHERE id = ?1",
            params![id, status, result, now_millis()],
        )?;
        Ok(())
    }

    /// The boot pass: a `running` row belonged to the process that just
    /// died and is failed as interrupted; a `queued` row gets its
    /// timestamp bumped, so the source's re-ingest carries a dedup id
    /// the previous process's consumed set cannot swallow. Returns the
    /// number of rows it touched.
    pub fn recover(&self) -> Result<usize, JobStoreError> {
        let conn = self.lock();
        let now = now_millis();
        let interrupted = conn.execute(
            "UPDATE coding_jobs
             SET status = 'failed',
                 result = 'interrupted: the daemon restarted mid-job; run it again',
                 updated_at_millis = ?1
             WHERE status = 'running'",
            params![now],
        )?;
        let requeued = conn.execute(
            "UPDATE coding_jobs SET updated_at_millis = ?1 WHERE status = 'queued'",
            params![now],
        )?;
        Ok(interrupted + requeued)
    }

    fn live_locked(
        conn: &rusqlite::Connection,
        instance: &str,
    ) -> Result<Option<JobRecord>, JobStoreError> {
        let mut statement = conn.prepare(
            "SELECT id, instance, prompt, status, result, created_at_millis, updated_at_millis
             FROM coding_jobs WHERE instance = ?1 AND status IN ('queued', 'running')
             ORDER BY id LIMIT 1",
        )?;
        let mut rows = statement.query_map(params![instance], row_job)?;
        rows.next().transpose().map_err(JobStoreError::from)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn row_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRecord> {
    Ok(InstanceRecord {
        name: row.get(0)?,
        path: row.get(1)?,
        session: row.get(2)?,
        created_at_millis: row.get(3)?,
    })
}

fn row_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: row.get(0)?,
        instance: row.get(1)?,
        prompt: row.get(2)?,
        status: JobStatus::parse(&row.get::<_, String>(3)?),
        result: row.get(4)?,
        created_at_millis: row.get(5)?,
        updated_at_millis: row.get(6)?,
    })
}

/// Milliseconds since the Unix epoch, the table's clock everywhere.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_lists_and_drops_instances() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("rhema", "/home/me/rhema").unwrap();
        store.add_instance("widgets", "/home/me/widgets").unwrap();
        let instances = store.instances().unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].name, "rhema");
        assert_eq!(instances[0].session, None);
        assert!(store.drop_instance("widgets").unwrap());
        assert_eq!(store.instances().unwrap().len(), 1);
        assert!(!store.drop_instance("widgets").unwrap(), "already gone");
    }

    #[test]
    fn names_the_key_cannot_hold_are_refused() {
        let store = CodingJobStore::in_memory().unwrap();
        for bad in ["", "a#b", "a/b", "a:b", "a b"] {
            let err = store.add_instance(bad, "/tmp").unwrap_err();
            assert!(matches!(err, JobStoreError::InvalidName { .. }), "{bad}");
        }
        store.add_instance("ok-name_1", "/tmp").unwrap();
    }

    #[test]
    fn a_duplicate_add_names_the_collision() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        let err = store.add_instance("rhema", "/tmp/other").unwrap_err();
        assert!(matches!(err, JobStoreError::Duplicate { .. }));
    }

    #[test]
    fn one_live_job_per_instance() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        let first = store.enqueue("rhema", "fix the lint").unwrap();
        assert_eq!(first.status, JobStatus::Queued);
        let err = store.enqueue("rhema", "also this").unwrap_err();
        assert!(matches!(err, JobStoreError::Busy { .. }));
        store.finish(first.id, "lint fixed").unwrap();
        // Done unblocks the next enqueue.
        store.enqueue("rhema", "run the tests").unwrap();
    }

    #[test]
    fn finishing_frees_but_a_second_instance_runs_parallel() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("a", "/tmp/a").unwrap();
        store.add_instance("b", "/tmp/b").unwrap();
        store.enqueue("a", "one").unwrap();
        store.enqueue("b", "two").unwrap();
        assert_eq!(store.queued().unwrap().len(), 2);
        assert!(store.live_job("a").unwrap().is_some());
    }

    #[test]
    fn enqueue_requires_the_instance_and_the_status_tool_finds_the_job() {
        let store = CodingJobStore::in_memory().unwrap();
        let err = store.enqueue("ghost", "x").unwrap_err();
        assert!(matches!(err, JobStoreError::UnknownInstance { .. }));
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        let job = store.enqueue("rhema", "do it").unwrap();
        let read = store.job(job.id).unwrap().unwrap();
        assert_eq!(read.prompt, "do it");
        assert_eq!(store.jobs_for("rhema", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_drop_is_refused_while_a_job_is_live() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        let job = store.enqueue("rhema", "long work").unwrap();
        store.mark_running(job.id).unwrap();
        let err = store.drop_instance("rhema").unwrap_err();
        assert!(matches!(err, JobStoreError::Busy { .. }));
        store.fail(job.id, "cancelled").unwrap();
        assert!(store.drop_instance("rhema").unwrap());
    }

    #[test]
    fn the_session_ride_transitions_marking_and_recover() {
        let store = CodingJobStore::in_memory().unwrap();
        store.add_instance("rhema", "/tmp/rhema").unwrap();
        store.save_session("rhema", r#"{"history":[]}"#).unwrap();
        assert_eq!(
            store.instance("rhema").unwrap().unwrap().session,
            Some(r#"{"history":[]}"#.to_owned())
        );
        store.clear_session("rhema").unwrap();
        assert_eq!(store.instance("rhema").unwrap().unwrap().session, None);
        store.save_session("ghost", "x").unwrap_err();

        let job = store.enqueue("rhema", "work").unwrap();
        store.mark_running(job.id).unwrap();
        assert_eq!(
            store.job(job.id).unwrap().unwrap().status,
            JobStatus::Running
        );
        // The boot pass: the running row is failed as interrupted; the
        // other instance's queued row only has its timestamp moved, so
        // its re-ingest after the restart carries a fresh dedup id.
        store.add_instance("other", "/tmp/other").unwrap();
        let queued = store.enqueue("other", "queued work").unwrap();
        let touched = store.recover().unwrap();
        assert_eq!(touched, 2);
        let after = store.job(job.id).unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Failed);
        assert!(after.result.unwrap().contains("interrupted"));
        assert!(!after.status.is_live());
        assert_eq!(
            store.job(queued.id).unwrap().unwrap().status,
            JobStatus::Queued,
            "still queued, only touched"
        );
    }
}
