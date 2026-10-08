//! The durable [`Store`]: SQLite, spec §7's minimum viable store.
//!
//! One row per [`ResourceKey`]. The whole [`KeyRecord`] travels as one JSON
//! document in the row, so a save is a single statement — whole-record
//! atomicity and the revision compare-and-swap fall out of SQLite's own
//! statement atomicity (`INSERT` for a must-not-exist create, `UPDATE ...
//! WHERE revision = ?` for a guarded write), and the `resource_key`,
//! `revision`, and `updated_at` columns stay queryable for ops, matching
//! the §7 table shape. [`VolatileStore`] and this backend are interchangeable
//! behind the [`Store`] seam; this one replays the exact behavioral suite
//! via the `store-contract` feature.
//!
//! Durability before return — the [`Store`] contract's last invariant — is
//! SQLite's: every save autocommits under `synchronous=FULL`, so a crash
//! after `save` returns cannot lose the transition. WAL mode keeps a
//! concurrent reader (ops queries, a second process) from blocking the
//! dispatcher's writes.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rusqlite::ErrorCode;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::StoreError;
use crate::key::ResourceKey;
use crate::state::Revision;
use crate::store::{KeyRecord, Store};

/// The dispatch table: spec §7's row shape, with the record as one JSON
/// document so whole-record atomicity is a property of the statement.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS key_record (
    resource_key TEXT PRIMARY KEY,
    revision     INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    record       TEXT NOT NULL
);
";

/// A [`Store`] over a SQLite database (a file, or an in-memory database
/// for tests and ephemeral use).
///
/// Shareable: one connection behind a mutex serializes every operation,
/// which is all the dispatcher asks of the seam. Independent
/// [`SqliteStore`]s on the same file (a second process, tests) interleave
/// through SQLite's own locking, and the revision compare-and-swap still
/// holds across them.
pub struct SqliteStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path).map_err(backend)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory database: the contract without the file.
    pub fn in_memory() -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(backend)?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, StoreError> {
        if durable {
            // WAL keeps a concurrent reader from blocking writes; FULL makes
            // every commit fsync before the statement returns, which is the
            // Store contract's durability-before-return invariant.
            let _mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(backend)?;
            conn.pragma_update(None, "synchronous", "FULL")
                .map_err(backend)?;
        }
        // A second store on the same file waits out the other's write lock
        // instead of surfacing SQLITE_BUSY as a backend failure.
        conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        conn.execute_batch(SCHEMA).map_err(backend)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Backend failures carry the engine's message; everything above the seam
/// (CAS, JSON) maps to its own variant.
fn backend(err: rusqlite::Error) -> StoreError {
    StoreError::Backend(err.to_string())
}

/// The stored revision for `key`: `0` when absent, matching
/// [`crate::VolatileStore`]'s spelling of a failed compare-and-swap against a
/// missing record.
fn stored_revision(conn: &rusqlite::Connection, key: &ResourceKey) -> Result<Revision, StoreError> {
    // SQLite integers are i64; revisions are u64 and never negative.
    let stored: Option<i64> = conn
        .query_row(
            "SELECT revision FROM key_record WHERE resource_key = ?1",
            params![key.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    Ok(stored.unwrap_or_default() as Revision)
}

impl Store for SqliteStore {
    fn load(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let payload: Option<String> = conn
            .query_row(
                "SELECT record FROM key_record WHERE resource_key = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload
            .map(|payload| serde_json::from_str(&payload))
            .transpose()
            .map_err(StoreError::from)
    }

    fn save(
        &self,
        record: KeyRecord,
        expected_revision: Option<Revision>,
    ) -> Result<(), StoreError> {
        let payload = serde_json::to_string(&record)?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let written: Result<usize, rusqlite::Error> = match expected_revision {
            None => conn
                .prepare_cached(
                    "INSERT INTO key_record (resource_key, revision, updated_at, record)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        record.key.as_str(),
                        record.revision as i64,
                        record.updated_at as i64,
                        payload,
                    ])
                }),
            Some(expected) => conn
                .prepare_cached(
                    "UPDATE key_record
                     SET revision = ?2, updated_at = ?3, record = ?4
                     WHERE resource_key = ?1 AND revision = ?5",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        record.key.as_str(),
                        record.revision as i64,
                        record.updated_at as i64,
                        payload,
                        expected as i64,
                    ])
                }),
        };
        match written {
            // One row touched: the statement, and therefore the
            // transition, is committed.
            Ok(1) => Ok(()),
            // Zero rows: the compare-and-swap precondition failed.
            Ok(_) => {
                let actual = stored_revision(&conn, &record.key)?;
                Err(StoreError::RevisionMismatch {
                    expected: expected_revision.unwrap_or_default(),
                    actual,
                })
            }
            // A create that hit an existing row is a compare-and-swap
            // failure like any other.
            Err(err) if err.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) => {
                let actual = stored_revision(&conn, &record.key)?;
                Err(StoreError::RevisionMismatch {
                    expected: expected_revision.unwrap_or_default(),
                    actual,
                })
            }
            Err(err) => Err(backend(err)),
        }
    }

    fn keys(&self) -> Result<Vec<ResourceKey>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn
            .prepare("SELECT resource_key FROM key_record ORDER BY resource_key ASC")
            .map_err(backend)?;
        let keys = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?
            .map(|row| Ok(ResourceKey::from(row?)))
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(backend)?;
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_sqlite() {
        let store = SqliteStore::in_memory().unwrap();
        let key = ResourceKey::from("github:acme/widgets#142");
        let mut record = KeyRecord::fresh(key.clone(), 7);
        record.revision = 3;
        store.save(record.clone(), None).unwrap();
        assert_eq!(store.load(&key).unwrap(), Some(record));
        assert_eq!(store.keys().unwrap(), vec![key]);
    }

    #[test]
    fn a_corrupted_row_surfaces_as_a_json_error() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO key_record (resource_key, revision, updated_at, record)
             VALUES ('k', 0, 0, 'not json')",
            [],
        )
        .unwrap();
        drop(conn);
        let err = store
            .load(&ResourceKey::from("k"))
            .expect_err("corrupt record is loud");
        assert!(matches!(err, StoreError::Json(_)), "got: {err:?}");
    }

    #[test]
    fn a_failed_cas_reports_the_stored_revision() {
        let store = SqliteStore::in_memory().unwrap();
        let key = ResourceKey::from("k");
        let mut record = KeyRecord::fresh(key.clone(), 0);
        record.revision = 1;
        store.save(record, None).unwrap();
        let mut stale = KeyRecord::fresh(key.clone(), 0);
        stale.revision = 2;
        let err = store
            .save(stale, Some(0))
            .expect_err("stale write rejected");
        match err {
            StoreError::RevisionMismatch {
                expected: 0,
                actual: 1,
            } => {}
            other => panic!("expected a revision mismatch, got: {other:?}"),
        }
    }
}
