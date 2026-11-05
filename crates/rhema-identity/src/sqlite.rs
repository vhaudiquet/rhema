//! The durable [`IdentityStore`]: one `accounts` table in the
//! deployment's SQLite file, its own connection beside the other stores'
//! — WAL and a busy timeout, the two-connections pattern the claw's
//! stores established. Secrets sit in the file's `secret` column; file
//! privacy is the consumer's job (0600 file, 0700 directory).

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
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
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
        .map_err(IdentityError::from)
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
                    kind: SecretKind::parse(&kind).unwrap_or_else(|| {
                        unreachable!("the CHECK constraint only admits 'token' and 'password'")
                    }),
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
        secret_kind: SecretKind::parse(&kind).unwrap_or_else(|| {
            unreachable!("the CHECK constraint only admits 'token' and 'password'")
        }),
        created_at: row.get::<_, i64>(6)? as u64,
        updated_at: row.get::<_, i64>(7)? as u64,
    })
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
            Err(err) => Err(IdentityError::from(err)),
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
    fn two_connections_share_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let writer = SqliteIdentityStore::open(&path).expect("first connection");
        let reader = SqliteIdentityStore::open(&path).expect("second connection");
        writer.put(draft("acme")).expect("put");
        assert_eq!(reader.list().expect("list").len(), 1);
    }
}
