//! The durable [`Store`]: SQLite, spec §7's minimum viable store.
//!
//! One row per [`ResourceKey`]. The whole [`KeyRecord`] travels as one JSON
//! document in the row, so a save is a single statement, whole-record
//! atomicity and the revision compare-and-swap fall out of SQLite's own
//! statement atomicity (`INSERT` for a must-not-exist create, `UPDATE ...
//! WHERE revision = ?` for a guarded write), and the `resource_key`,
//! `revision`, and `updated_at` columns stay queryable for ops, matching
//! the §7 table shape. [`VolatileStore`](crate::VolatileStore) and this backend are interchangeable
//! behind the [`Store`] seam; this one replays the exact behavioral suite
//! via the `store-contract` feature.
//!
//! Durability before return, the [`Store`] contract's last invariant, is
//! SQLite's: every save autocommits under `synchronous=FULL`, so a crash
//! after `save` returns cannot lose the transition. WAL mode keeps a
//! concurrent reader (ops queries, a second process) from blocking the
//! dispatcher's writes.
//!
//! The table carries a version in `PRAGMA user_version`, stamped at open:
//! a file stamped by a newer build refuses to open rather than write to a
//! schema it does not fully understand.

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

/// The schema version this build reads and writes, stamped as the
/// database's `PRAGMA user_version`. Version 1 is the first stamped
/// shape: before the stamp shipped, every file this store opened held
/// this same table, so an unstamped (`0`) file is a legacy one, not a
/// different schema.
const VERSION: u32 = 1;

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
        // The stamp check is a plain read, and it must happen before the
        // pragmas below: WAL itself writes the header, and a file from a
        // newer build must open the door to nothing at all, not even a
        // journal-mode flip.
        migrate(&conn)?;
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

/// Reads the database's schema version stamp and reconciles it with
/// [`VERSION`], the one seam a future schema bump has.
///
/// `0` is a brand-new database or a legacy file from before versions
/// were stamped: both hold today's exact shape, so both take the stamp.
/// The check stays a plain read followed by one write for the `0` case,
/// and that is all it can be here: the dispatcher's restart path
/// ([`crate::Dispatcher::recover_all`]) already assumes this process is
/// the only live one over the store, and the stamp asks for no more
/// than that same single-node world. The one interleaving independent
/// openers can produce is both reading `0` and both writing the same
/// `1`; SQLite serializes the statements, the value is the same, and no
/// dispatch or revision state rides on it. A version above [`VERSION`]
/// was written by a newer binary: refuse before writing a thing, so
/// this build cannot corrupt a file it does not fully understand. A
/// bump grows a match arm here, from the older version upward; none
/// has shipped, so `0` is the whole ladder.
fn migrate(conn: &rusqlite::Connection) -> Result<(), StoreError> {
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(backend)?;
    if found < 0 || found > i64::from(VERSION) {
        return Err(StoreError::Backend(format!(
            "dispatch database schema version {found} is not supported by \
             this build (its version is {VERSION}); refusing to open"
        )));
    }
    if found == 0 {
        conn.pragma_update(None, "user_version", VERSION)
            .map_err(backend)?;
    }
    Ok(())
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

    fn delete(&self, key: &ResourceKey) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        // One statement, autocommitted: the row is gone before this
        // returns, which is the contract's durability clause for a delete.
        let removed = conn
            .prepare_cached("DELETE FROM key_record WHERE resource_key = ?1")
            .and_then(|mut stmt| stmt.execute(params![key.as_str()]))
            .map_err(backend)?;
        // `resource_key` is the primary key: at most one row per delete.
        Ok(removed > 0)
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
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

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

    /// The unit tests need file-backed databases here, and this crate
    /// keeps no tempdir dev-dependency: the pattern of
    /// `tests/sqlite_store.rs` serves, a unique name under the shared
    /// temp dir, removed with its WAL siblings when the guard drops.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "rhema-rsd-sqlite-{label}-{n}-{pid}.db",
                pid = std::process::id()
            )))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                drop(std::fs::remove_file(sibling(&self.0, suffix)));
            }
        }
    }

    /// `<path><suffix>`, SQLite's naming for the WAL and shared-memory
    /// siblings.
    fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn user_version(store: &SqliteStore) -> i64 {
        let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_fresh_database_takes_the_stamp_and_round_trips() {
        let db = TempDb::new("stamped");
        let store = SqliteStore::open(db.path()).unwrap();
        let key = ResourceKey::from("github:acme/widgets#142");
        let mut record = KeyRecord::fresh(key.clone(), 7);
        record.revision = 3;
        store.save(record.clone(), None).unwrap();
        assert_eq!(store.load(&key).unwrap(), Some(record));
        assert_eq!(store.keys().unwrap(), vec![key]);
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_legacy_database_opens_reads_its_row_and_takes_the_stamp() {
        let db = TempDb::new("legacy");
        // The file on disk before this release stamped versions: the
        // table, one row, and `user_version` left at its 0 default.
        let key = ResourceKey::from("github:acme/widgets#142");
        {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            let record = KeyRecord::fresh(key.clone(), 7);
            conn.execute(
                "INSERT INTO key_record (resource_key, revision, updated_at, record) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    key.as_str(),
                    record.revision as i64,
                    record.updated_at as i64,
                    serde_json::to_string(&record).unwrap()
                ],
            )
            .unwrap();
        }
        let store = SqliteStore::open(db.path()).unwrap();
        assert_eq!(
            store.load(&key).unwrap(),
            Some(KeyRecord::fresh(key.clone(), 7))
        );
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_file_a_newer_build_stamped_refuses_to_open_untouched() {
        let db = TempDb::new("newer");
        {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let before = std::fs::read(db.path()).unwrap();
        let message = match SqliteStore::open(db.path()) {
            Err(StoreError::Backend(message)) => message,
            Err(other) => panic!("expected the backend refusal, got {other:?}"),
            Ok(_) => panic!("a file from a newer build opened"),
        };
        // The refusal names both sides of the gap, so the operator
        // knows which binary is the old one.
        for expected in ["schema version 2", "its version is 1", "not supported"] {
            assert!(message.contains(expected), "message: {message:?}");
        }
        assert_eq!(std::fs::read(db.path()).unwrap(), before);
        assert!(!sibling(db.path(), "-wal").exists());
        assert!(!sibling(db.path(), "-shm").exists());
        // Still stamped 2: the refusal wrote nothing, not the stamp.
        {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, 2);
        }
    }
}
