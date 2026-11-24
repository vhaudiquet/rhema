//! The durable [`IdentityStore`]: one `accounts` table in the
//! deployment's SQLite file, its own connection beside the other stores'
//!, WAL and a busy timeout, the two-connections pattern the claw's
//! stores established. Secrets sit in the file's `secret` column; file
//! privacy is the consumer's job (0600 file, 0700 directory).
//!
//! The table is versioned as `PRAGMA user_version`, stamped at open: a
//! file stamped by a newer build refuses to open rather than write to a
//! schema it does not fully understand.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::params;

use crate::record::{Account, AccountDraft, Secret, SecretKind, check_id};
use crate::store::{IdentityError, IdentityStore};

/// The accounts table. The CHECK pins the kind column to what
/// [`SecretKind`] writes.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    service     TEXT NOT NULL,
    url         TEXT,
    username    TEXT,
    secret_kind TEXT NOT NULL CHECK(secret_kind IN ('token','password')),
    secret      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
";

/// The schema version this build reads and writes, stamped as the
/// database's `PRAGMA user_version`. Version 1 is the first stamped
/// shape: before the stamp shipped, every file this store opened held
/// this same table, so an unstamped (`0`) file is a legacy one, not a
/// different schema.
const VERSION: u32 = 1;

/// The durable identity store over its own connection to a SQLite file.
/// Shareable behind an `Arc`; every operation takes `&self`.
pub struct SqliteIdentityStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteIdentityStore {
    /// Opens (creating if needed) the accounts table in the database at
    /// `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IdentityError> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory table: the shape without the file, for
    /// tests that need no sharing.
    pub fn in_memory() -> Result<Self, IdentityError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, IdentityError> {
        // The stamp check is a plain read, and it must happen before the
        // WAL switch below: that pragma writes the header, and a file
        // from a newer build must open the door to nothing at all, not
        // even a journal-mode flip.
        migrate(&conn)?;
        if durable {
            // Same pragmas as the other stores on the same file: SQLite
            // stores the journal mode per database, and every connection
            // must agree that it is WAL.
            let _mode: String =
                conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
        }
        conn.execute_batch(SCHEMA)?;
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
/// Racing openers each read `0` and each write the same `1`; the write
/// is idempotent, so no interleaving can matter. A version above
/// [`VERSION`] was written by a newer binary: refuse before writing a
/// thing, so this build cannot corrupt a file it does not fully
/// understand. A bump grows a match arm here, from the older version
/// upward; none has shipped, so `0` is the whole ladder.
fn migrate(conn: &rusqlite::Connection) -> Result<(), IdentityError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if found < 0 || found > i64::from(VERSION) {
        return Err(IdentityError::Backend(Box::new(SchemaMismatch {
            found,
            supported: VERSION,
        })));
    }
    if found == 0 {
        conn.pragma_update(None, "user_version", VERSION)?;
    }
    Ok(())
}

impl IdentityStore for SqliteIdentityStore {
    fn put(&self, draft: AccountDraft) -> Result<Account, IdentityError> {
        let norm = draft.validate()?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let now_ms = now_millis();
        let now = now_ms as i64;
        match draft.id {
            Some(id) => {
                let created = created_at_of(&conn, &id)?;
                let updated = conn.execute(
                    "UPDATE accounts
                     SET label = ?2, service = ?3, url = ?4, username = ?5,
                         secret_kind = ?6, secret = ?7, updated_at = ?8
                     WHERE id = ?1",
                    params![
                        id,
                        norm.label,
                        norm.service,
                        norm.url,
                        norm.username,
                        norm.secret.kind.name(),
                        norm.secret.value,
                        now
                    ],
                )?;
                if updated != 1 {
                    return Err(IdentityError::UnknownId { id });
                }
                Ok(Account {
                    id,
                    label: norm.label,
                    service: norm.service,
                    url: norm.url,
                    username: norm.username,
                    secret_kind: norm.secret.kind,
                    created_at: created,
                    updated_at: now_ms,
                })
            }
            None => {
                let id = loop {
                    let candidate = fresh_id();
                    let taken: bool = conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
                            params![candidate],
                            |row| row.get(0),
                        )
                        .map_err(IdentityError::from)?;
                    if !taken {
                        break candidate;
                    }
                };
                conn.execute(
                    "INSERT INTO accounts
                     (id, label, service, url, username, secret_kind, secret, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        id,
                        norm.label,
                        norm.service,
                        norm.url,
                        norm.username,
                        norm.secret.kind.name(),
                        norm.secret.value,
                        now
                    ],
                )?;
                Ok(Account {
                    id,
                    label: norm.label,
                    service: norm.service,
                    url: norm.url,
                    username: norm.username,
                    secret_kind: norm.secret.kind,
                    created_at: now_ms,
                    updated_at: now_ms,
                })
            }
        }
    }

    fn get(&self, id: &str) -> Result<Option<Account>, IdentityError> {
        check_id(id)?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row(
            "SELECT id, label, service, url, username, secret_kind, created_at, updated_at
             FROM accounts WHERE id = ?1",
            params![id],
            row_account,
        )
        .optional_or_none()
    }

    fn list(&self) -> Result<Vec<Account>, IdentityError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn.prepare(
            "SELECT id, label, service, url, username, secret_kind, created_at, updated_at
             FROM accounts ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], row_account)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(identity)?;
        Ok(rows)
    }

    fn forget(&self, id: &str) -> Result<bool, IdentityError> {
        check_id(id)?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        Ok(removed == 1)
    }

    fn set_secret(&self, id: &str, secret: Secret) -> Result<Account, IdentityError> {
        check_id(id)?;
        let value = secret.value.trim();
        if value.is_empty() {
            return Err(IdentityError::EmptySecret);
        }
        if value.len() > crate::SECRET_LIMIT {
            return Err(IdentityError::SecretTooLong {
                len: value.len(),
                limit: crate::SECRET_LIMIT,
            });
        }
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let now = now_millis();
        let updated = conn.execute(
            "UPDATE accounts SET secret_kind = ?2, secret = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, secret.kind.name(), value, now as i64],
        )?;
        if updated != 1 {
            return Err(IdentityError::UnknownId { id: id.to_owned() });
        }
        conn.query_row(
            "SELECT id, label, service, url, username, secret_kind, created_at, updated_at
             FROM accounts WHERE id = ?1",
            params![id],
            row_account,
        )
        .map_err(identity)
    }

    fn secret(&self, id: &str) -> Result<Option<Secret>, IdentityError> {
        check_id(id)?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row(
            "SELECT secret_kind, secret FROM accounts WHERE id = ?1",
            params![id],
            |row| {
                let kind: String = row.get(0)?;
                Ok(Secret {
                    kind: known_kind(kind)?,
                    value: row.get(1)?,
                })
            },
        )
        .optional_or_none()
    }
}

/// Maps one redacted account row; the shared tail of the read paths.
/// Timestamps ride the column as i64 (rusqlite has no u64 binding) and
/// widen to the store's unsigned convention.
fn row_account(row: &rusqlite::Row<'_>) -> Result<Account, rusqlite::Error> {
    let kind: String = row.get(5)?;
    Ok(Account {
        id: row.get(0)?,
        label: row.get(1)?,
        service: row.get(2)?,
        url: row.get(3)?,
        username: row.get(4)?,
        secret_kind: known_kind(kind)?,
        created_at: row.get::<_, i64>(6)? as u64,
        updated_at: row.get::<_, i64>(7)? as u64,
    })
}

/// Decodes the `secret_kind` column, naming the one bad value a row can
/// carry: the table was hand-edited past its CHECK constraint, and an
/// unknown spelling is the store's [`IdentityError::Corrupt`], never a
/// panic. The text travels through the one channel the row mapper has,
/// rusqlite's conversion failure, and widens at the store's edge.
fn known_kind(raw: String) -> Result<SecretKind, rusqlite::Error> {
    SecretKind::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(UnknownKind(raw)),
        )
    })
}

/// The marker [`known_kind`] ships inside the row failure, how
/// [`identity`] tells a hand-edited kind from every other conversion
/// failure.
#[derive(Debug)]
struct UnknownKind(String);

impl std::fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "secret kind {:?} is not one this version knows", self.0)
    }
}

impl std::error::Error for UnknownKind {}

/// The failure `migrate` rides inside [`IdentityError::Backend`]: the
/// file's `user_version` stamp names a schema a newer build wrote. The
/// message carries both sides of the gap, the stamp found and the
/// version this build supports.
#[derive(Debug)]
struct SchemaMismatch {
    /// The stamp the file carries.
    found: i64,
    /// The version this build reads and writes.
    supported: u32,
}

impl std::fmt::Display for SchemaMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "accounts database schema version {} is not supported by this build \
             (its version is {}); refusing to open",
            self.found, self.supported
        )
    }
}

impl std::error::Error for SchemaMismatch {}

/// Widens a row failure to the store's error: a hand-edited kind reads
/// as [`IdentityError::Corrupt`], everything else keeps its backend
/// shape.
fn identity(err: rusqlite::Error) -> IdentityError {
    if let rusqlite::Error::FromSqlConversionFailure(_, _, inner) = &err
        && let Some(UnknownKind(kind)) = inner.downcast_ref::<UnknownKind>()
    {
        return IdentityError::Corrupt {
            kind: kind.to_owned(),
        };
    }
    IdentityError::from(err)
}

/// The `created_at` of an existing row, for update paths that must
/// preserve it. An absent row is `UnknownId`; a real backend failure is
/// never masqueraded as one.
fn created_at_of(conn: &rusqlite::Connection, id: &str) -> Result<u64, IdentityError> {
    match conn.query_row(
        "SELECT created_at FROM accounts WHERE id = ?1",
        params![id],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(millis) => Ok(millis as u64),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(IdentityError::UnknownId { id: id.to_owned() })
        }
        Err(err) => Err(IdentityError::from(err)),
    }
}

/// Wall-clock millis since the Unix epoch. The volatile store keeps this
/// behind its lock; here each write stamps its own instant.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A fresh store-assigned id candidate; collisions (a counter wrap, or a
/// second process lifetime in the same second) retry in the caller.
fn fresh_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = now_millis();
    let secs = (millis / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    // Compact UTC stamp; the civil-from-days arithmetic mirrors
    // `rhema-memory`'s id shape ({UTC second}-{counter:08x}).
    let (y, m, d) = {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        // Hinnant's year correction: the era math lines the civil year
        // up with March, so January and February belong to the NEXT
        // year of the computed pair. Without it every Jan/Feb id
        // reads a year early (record.rs and rhema-memory carry the
        // same line).
        (year + i64::from(month <= 2), month, day)
    };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}-{n:08x}",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// `query_row`'s "no rows" is `QueryReturnedNoRows`, which the reads map
/// to `None` rather than an error.
trait OptionalRow<T> {
    fn optional_or_none(self) -> Result<Option<T>, IdentityError>;
}

impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional_or_none(self) -> Result<Option<T>, IdentityError> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(identity(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::SecretKind;
    use crate::store_contract;

    fn draft(label: &str) -> AccountDraft {
        AccountDraft::new(label, "github", Secret::new(SecretKind::Token, "tok"))
    }

    #[test]
    fn the_contract_holds() {
        store_contract::run_all(&SqliteIdentityStore::in_memory().expect("store opens"));
    }

    #[test]
    fn secrets_survive_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let account = {
            let store = SqliteIdentityStore::open(&path).expect("opens");
            store.put(draft("acme")).expect("put")
        };
        let reopened = SqliteIdentityStore::open(&path).expect("reopens");
        assert_eq!(
            reopened.get(&account.id).expect("get"),
            Some(account.clone())
        );
        let secret = reopened
            .secret(&account.id)
            .expect("secret")
            .expect("stored");
        assert_eq!(secret.value, "tok");
    }

    #[test]
    fn an_unknown_kind_surfaces_as_a_corrupt_row_not_a_panic() {
        // The way a hand-edited table actually arrives: the row is in the
        // file and its `secret_kind` is not one this version writes. The
        // CHECK constraint is sidestepped on the way in, as an edit
        // outside the store would sidestep it.
        let store = SqliteIdentityStore::in_memory().expect("store opens");
        let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             INSERT INTO accounts
                 (id, label, service, url, username, secret_kind, secret,
                  created_at, updated_at)
             VALUES
                 ('20260101T000000-00000000', 'acme', 'github', NULL, NULL,
                  'quux', 'tok', 0, 0);",
        )
        .expect("the hand edit lands");
        drop(conn);
        let id = "20260101T000000-00000000";
        for err in [
            store.get(id).expect_err("the read is loud"),
            store.secret(id).expect_err("the secret is loud"),
            // `list` fails on the row itself, the whole table is the one
            // hand edit away from unreadable.
            store.list().expect_err("the listing is loud"),
        ] {
            assert!(
                matches!(err, IdentityError::Corrupt { ref kind } if kind == "quux"),
                "{err:?}"
            );
        }
    }

    #[test]
    fn two_connections_share_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let writer = SqliteIdentityStore::open(&path).expect("first connection");
        let reader = SqliteIdentityStore::open(&path).expect("second connection");
        writer.put(draft("acme")).expect("put");
        assert_eq!(reader.list().expect("list").len(), 1);
    }

    fn user_version(store: &SqliteIdentityStore) -> i64 {
        let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("the stamp reads")
    }

    #[test]
    fn a_fresh_database_takes_the_stamp_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let store = SqliteIdentityStore::open(&path).expect("opens");
        let account = store.put(draft("acme")).expect("put");
        assert_eq!(store.get(&account.id).expect("get").expect("row"), account);
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_legacy_database_opens_reads_its_row_and_takes_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        // The file on disk before this release stamped versions: the
        // table, one row, and `user_version` left at its 0 default.
        {
            let conn = rusqlite::Connection::open(&path).expect("hand open");
            conn.execute_batch(SCHEMA).expect("hand schema");
            conn.execute(
                "INSERT INTO accounts \
                 (id, label, service, url, username, secret_kind, secret,
                  created_at, updated_at) \
                 VALUES ('legacy', 'acme', 'github', NULL, NULL, 'token', 'tok', 0, 0)",
                [],
            )
            .expect("hand row");
        }
        let store = SqliteIdentityStore::open(&path).expect("opens");
        assert_eq!(
            store.get("legacy").expect("get").expect("row"),
            Account {
                id: "legacy".into(),
                label: "acme".into(),
                service: "github".into(),
                url: None,
                username: None,
                secret_kind: SecretKind::Token,
                created_at: 0,
                updated_at: 0,
            }
        );
        assert_eq!(
            store.secret("legacy").expect("secret").expect("row").value,
            "tok"
        );
        assert_eq!(user_version(&store), 1);
    }

    #[test]
    fn a_file_a_newer_build_stamped_refuses_to_open_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("hand open");
            conn.execute_batch(SCHEMA).expect("hand schema");
            conn.pragma_update(None, "user_version", 2).expect("stamp");
        }
        let before = std::fs::read(&path).expect("file reads");
        let message = match SqliteIdentityStore::open(&path) {
            Err(IdentityError::Backend(err)) => err.to_string(),
            Err(other) => panic!("expected the backend refusal, got {other:?}"),
            Ok(_) => panic!("a file from a newer build opened"),
        };
        // The refusal names both sides of the gap, so the operator
        // knows which binary is the old one.
        for expected in ["schema version 2", "its version is 1", "not supported"] {
            assert!(message.contains(expected), "message: {message:?}");
        }
        assert_eq!(std::fs::read(&path).expect("file reads"), before);
        assert!(!dir.path().join("state.db-wal").exists());
        // Still stamped 2: the refusal wrote nothing, not the stamp.
        {
            let conn = rusqlite::Connection::open(&path).expect("hand reopen");
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("the stamp reads");
            assert_eq!(version, 2);
        }
    }
}
