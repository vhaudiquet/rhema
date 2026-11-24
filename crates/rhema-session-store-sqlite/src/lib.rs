//! `rhema-session-store-sqlite`, the durable [`SessionStore`] backend.
//!
//! One row per session: the four metadata columns a picker needs and the
//! body as the same pretty JSON the file format holds. A row can be
//! exported to a `.json` and resumed, and a body that does not parse
//! reports the same [`StoreError::Corrupt`] a file would.
//!
//! The database opens with `journal_mode=WAL`, `synchronous=FULL`, and a
//! busy timeout, the same settings `rhema-rsd`'s store uses. `save`
//! returns after the transaction commits: a crash then cannot lose the
//! record. `opening_prompt` is computed once, at save, so `list` reads
//! four columns and never parses a body.
//!
//! The schema carries a version in `PRAGMA user_version`, stamped at
//! open. A file stamped with a newer version was written by a newer
//! build; it refuses to open rather than write to a shape this build does
//! not fully understand.
//!
//! The crate exists so a file-only frontend never compiles or links
//! SQLite; a consumer that wants this backend depends on this crate and
//! no other frontend changes.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rhema_core::Session;
use rhema_session_store::{Meta, SessionId, SessionStore, StoreError, opening_prompt};
use rusqlite::{Connection, OptionalExtension, params};

/// The table shape: metadata columns queryable, the session as the file
/// format's JSON.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    saved_at       INTEGER NOT NULL,
    bytes          INTEGER NOT NULL,
    opening_prompt TEXT,
    body           TEXT NOT NULL
) STRICT;
";

/// The schema version this build reads and writes, stamped as the
/// database's `PRAGMA user_version`. Version 1 is the first stamped
/// shape: before the stamp shipped, every file this crate opened held
/// this same table, so an unstamped (`0`) file is a legacy one, not a
/// different schema.
const VERSION: u32 = 1;

/// One upsert: whole-record replacement in one statement.
const UPSERT: &str = "
INSERT INTO sessions (id, saved_at, bytes, opening_prompt, body)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(id) DO UPDATE SET
    saved_at       = excluded.saved_at,
    bytes          = excluded.bytes,
    opening_prompt = excluded.opening_prompt,
    body           = excluded.body
";

/// A [`SessionStore`] over one SQLite database.
///
/// One connection behind a mutex serializes this process's operations;
/// independent stores on the same file interleave through SQLite's own
/// locking.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// The database file; `None` for an in-memory store.
    path: Option<PathBuf>,
    /// Whether the `-wal`/`-shm` siblings have been tightened; they only
    /// exist after the first write.
    wal_tightened: AtomicBool,
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path).map_err(backend)?;
        let store = Self::init(conn, Some(path))?;
        store.tighten_database();
        Ok(store)
    }

    /// A private in-memory database: the contract without the file.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory().map_err(backend)?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self, StoreError> {
        // The stamp check is a plain read, and it must happen before
        // the pragmas below: WAL itself writes the header, and a file
        // from a newer build must open the door to nothing at all, not
        // even a journal-mode flip.
        migrate(&conn)?;
        // WAL keeps a concurrent reader from blocking the save path;
        // FULL fsyncs every commit before the statement returns.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(backend)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(backend)?;
        // A second writer waits out the other's lock instead of
        // surfacing SQLITE_BUSY.
        conn.busy_timeout(Duration::from_secs(5)).map_err(backend)?;
        conn.execute_batch(SCHEMA).map_err(backend)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
            wal_tightened: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tightens the database after open. Best effort: SQLite writes it
    /// with the process umask, and a filesystem that refuses mode
    /// changes must not fail the call.
    fn tighten_database(&self) {
        if let Some(path) = &self.path {
            tighten(path);
        }
    }

    /// Tightens the database and the WAL siblings once they exist, on
    /// the first save. Best effort, same contract as the database.
    fn tighten_siblings_once(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if self.wal_tightened.swap(true, Ordering::Relaxed) {
            return;
        }
        tighten(path);
        let wal = sibling(path, "-wal");
        let shm = sibling(path, "-shm");
        tighten(&wal);
        tighten(&shm);
    }
}

impl SessionStore for SqliteStore {
    fn load(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        let conn = self.lock();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM sessions WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        match body {
            None => Ok(None),
            Some(raw) => {
                serde_json::from_str(&raw)
                    .map(Some)
                    .map_err(|source| StoreError::Corrupt {
                        id: id.clone(),
                        source,
                    })
            }
        }
    }

    fn save(&self, id: &SessionId, session: &Session) -> Result<(), StoreError> {
        let body = serde_json::to_string_pretty(session)
            .map_err(|err| StoreError::Backend(format!("cannot serialize session: {err}")))?;
        let saved_at = now_millis();
        let bytes = body.len() as u64;
        let prompt = opening_prompt(session);
        let conn = self.lock();
        conn.execute(
            UPSERT,
            params![
                id.as_str(),
                saved_at as i64,
                bytes as i64,
                prompt,
                body.as_str()
            ],
        )
        .map_err(backend)?;
        drop(conn);
        self.tighten_siblings_once();
        Ok(())
    }

    fn delete(&self, id: &SessionId) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id.as_str()])
            .map_err(backend)?;
        Ok(changed > 0)
    }

    fn list(&self) -> Result<Vec<Meta>, StoreError> {
        let conn = self.lock();
        let mut statement = conn
            .prepare(
                "SELECT id, saved_at, bytes, opening_prompt FROM sessions \
                 ORDER BY saved_at DESC, id ASC",
            )
            .map_err(backend)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(backend)?;
        let mut out = Vec::new();
        for row in rows {
            let (raw_id, saved_at, bytes, opening_prompt) = row.map_err(backend)?;
            // Rows are written through `save`, so the id validates; a row
            // inserted by hand that does not is skipped rather than
            // failing the listing.
            let Ok(id) = SessionId::new(raw_id) else {
                continue;
            };
            out.push(Meta {
                id,
                saved_at: saved_at.max(0) as u64,
                bytes: bytes.max(0) as u64,
                opening_prompt,
            });
        }
        Ok(out)
    }
}

/// Reads the database's schema version stamp and reconciles it with
/// [`VERSION`], the one seam a future schema bump has.
///
/// `0` is a brand-new database or a legacy file from before versions
/// were stamped: both hold today's exact shape, so both take the stamp.
/// Racing openers each read `0` and each write the same `1`; the write
/// is idempotent, so no interleaving can matter. A version above
/// [`VERSION`] was written by a newer binary: refuse before writing a
/// thing, so this build cannot corrupt a file it does not fully
/// understand. A bump grows a match arm here, from the older version
/// upward; none has shipped, so `0` is the whole ladder.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(backend)?;
    if found < 0 || found > i64::from(VERSION) {
        return Err(StoreError::Backend(format!(
            "sessions database schema version {found} is not supported by \
             this build (its version is {VERSION}); refusing to open"
        )));
    }
    if found == 0 {
        conn.pragma_update(None, "user_version", VERSION)
            .map_err(backend)?;
    }
    Ok(())
}

/// Backend failures carry SQLite's message.
fn backend(err: rusqlite::Error) -> StoreError {
    StoreError::Backend(err.to_string())
}

/// Milliseconds since the epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// `<path><suffix>`, SQLite's naming for the WAL and shared-memory files.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Best-effort `0600` on a file this store owns.
#[cfg(unix)]
fn tighten(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// No mode bits to tighten here.
#[cfg(not(unix))]
fn tighten(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Message, Role};
    use rhema_session_store::contract;

    fn session_with(prompt: &str) -> Session {
        Session {
            history: vec![
                Message::text(Role::User, prompt),
                Message::text(Role::Assistant, "done"),
            ],
            cost_usd: 0.5,
            usage_stats: serde_json::json!({ "requests": 1 }),
            ..Session::default()
        }
    }

    fn temp_store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("sessions.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn the_contract_suite_passes_on_a_file() {
        let (_dir, store) = temp_store();
        contract::run_all(&store);
    }

    #[test]
    fn the_contract_suite_passes_in_memory() {
        contract::run_all(&SqliteStore::in_memory().unwrap());
    }

    #[test]
    fn wal_and_full_synchronous_are_set() {
        let (_dir, store) = temp_store();
        let conn = store.lock();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        // 2 is SQLITE_SYNC_FULL.
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sync, 2);
    }

    #[test]
    fn listing_never_reads_the_body() {
        let (_dir, store) = temp_store();
        // A row whose body cannot parse proves the listing does not
        // touch it; a query plan assertion would pin the SQL text
        // instead of the behavior.
        {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO sessions (id, saved_at, bytes, opening_prompt, body) \
                 VALUES ('bad-body', 7, 3, NULL, '{not json')",
                [],
            )
            .unwrap();
        }
        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_str(), "bad-body");
        assert_eq!(rows[0].opening_prompt, None);
        let id = SessionId::new("bad-body").unwrap();
        assert!(matches!(store.load(&id), Err(StoreError::Corrupt { .. })));
    }

    #[test]
    fn delete_reports_its_change_count() {
        let (_dir, store) = temp_store();
        let id = SessionId::new("gone").unwrap();
        store.save(&id, &session_with("gone")).unwrap();
        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn the_database_lands_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = temp_store();
        let path = dir.path().join("sessions.db");
        let id = SessionId::new("private").unwrap();
        store.save(&id, &session_with("private")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&sibling(&path, "-wal")), 0o600);
    }

    #[test]
    fn a_file_store_and_sqlite_round_trip_equal() {
        use rhema_session_store::SessionStore as _;
        use rhema_session_store_file::FileStore;

        let dir = tempfile::tempdir().unwrap();
        let file = FileStore::new(dir.path().join("files"));
        let sqlite = SqliteStore::open(dir.path().join("sessions.db")).unwrap();
        let id = SessionId::new("cross-backend").unwrap();
        let session = session_with("same value");
        file.save(&id, &session).unwrap();
        sqlite.save(&id, &session).unwrap();
        assert_eq!(file.load(&id).unwrap(), sqlite.load(&id).unwrap());
        assert_eq!(file.load(&id).unwrap(), Some(session));
    }

    #[test]
    fn a_row_dumped_to_a_file_resumes() {
        use rhema_session_store_file::PathStore;

        let (dir, store) = temp_store();
        let id = SessionId::new("dump").unwrap();
        let session = session_with("export me");
        store.save(&id, &session).unwrap();
        let exported = store.load(&id).unwrap().unwrap();
        let path = dir.path().join("exported.json");
        std::fs::write(&path, serde_json::to_string_pretty(&exported).unwrap()).unwrap();
        assert_eq!(PathStore::new(&path).load().unwrap(), Some(session));
    }

    #[test]
    fn a_database_reopens_with_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let id = SessionId::new("durable").unwrap();
        {
            let store = SqliteStore::open(&path).unwrap();
            store.save(&id, &session_with("still here")).unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.load(&id).unwrap(), Some(session_with("still here")));
        assert_eq!(store.list().unwrap().len(), 1);
    }

    fn user_version(store: &SqliteStore) -> i64 {
        store
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_fresh_database_takes_the_stamp_and_round_trips() {
        let (_dir, store) = temp_store();
        let id = SessionId::new("fresh").unwrap();
        store.save(&id, &session_with("stamp on open")).unwrap();
        assert_eq!(
            store.load(&id).unwrap(),
            Some(session_with("stamp on open"))
        );
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_legacy_database_opens_reads_its_row_and_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        // The file on disk before this release stamped versions: the
        // table, one row, and `user_version` left at its 0 default.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            let body = serde_json::to_string(&session_with("stored before stamps")).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, saved_at, bytes, opening_prompt, body) \
                 VALUES ('legacy', 7, 3, NULL, ?1)",
                params![body],
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let id = SessionId::new("legacy").unwrap();
        assert_eq!(
            store.load(&id).unwrap(),
            Some(session_with("stored before stamps"))
        );
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_file_a_newer_build_stamped_refuses_to_open_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, saved_at, bytes, opening_prompt, body) \
                 VALUES ('kept', 7, 3, NULL, '{}')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let message = match SqliteStore::open(&path) {
            Err(StoreError::Backend(message)) => message,
            Err(other) => panic!("expected the backend refusal, got {other:?}"),
            Ok(_) => panic!("a file from a newer build opened"),
        };
        // The refusal names the mismatch and the supported version
        // both, so the operator knows which side is old.
        for expected in ["schema version 2", "its version is 1", "not supported"] {
            assert!(message.contains(expected), "message: {message:?}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!sibling(&path, "-wal").exists());
        assert!(!sibling(&path, "-shm").exists());
    }
}
