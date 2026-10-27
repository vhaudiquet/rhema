//! The turn-history table: one durable row per streamed event, in the
//! same database file the dispatcher's
//! [`SqliteStore`](rhema_rsd::SqliteStore) and the other claw tables
//! use.
//!
//! The table is append-only and unbounded, on purpose: it keeps the
//! *transcript* — what a viewer saw, tool rows and thinking traces
//! included — and a resident agent's sessions run for weeks, so a
//! rolling window would eat history no client could ever get back. The
//! event hub appends every event here as it streams and replays from
//! here on subscribe; the session stash keeps the *conversation* (what
//! the model sees). A pre-window table (`turn_history`, one JSON blob
//! per key rewritten whole) migrates once, at open, into this shape.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::params;
use serde_json::Value;

use crate::hub::TurnEvent;

/// The turn-history table.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS turn_events (
    key   TEXT NOT NULL,
    seq   INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (key, seq)
);
";

/// Why a turn-history operation failed. A row whose JSON does not parse
/// is its own variant: a transcript that cannot replay must say so, not
/// silently read as "no history".
#[derive(Debug, thiserror::Error)]
pub enum TurnStoreError {
    /// The SQLite backend failed.
    #[error("turn history backend: {0}")]
    Backend(#[from] rusqlite::Error),
    /// A stored event is not a turn event.
    #[error("turn history for {key} does not parse: {source}")]
    Corrupt {
        /// The key whose row failed.
        key: String,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
}

/// The persistent turn-history table over its own connection to the
/// daemon's SQLite file. Shareable behind an `Arc`; every operation
/// takes `&self`.
pub struct TurnStore {
    conn: Mutex<rusqlite::Connection>,
}

impl TurnStore {
    /// Opens (creating if needed) the turn-history table in the database
    /// at `path`, migrating a legacy whole-blob table if one is present.
    /// WAL keeps the connections from blocking each other's readers; the
    /// busy timeout rides out the other's write locks.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TurnStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(TurnStoreError::from)?;
        Self::init(conn, true)
    }

    /// Opens a private in-memory table: the shape without the file, for
    /// tests that need no sharing.
    pub fn in_memory() -> Result<Self, TurnStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(TurnStoreError::from)?;
        Self::init(conn, false)
    }

    fn init(conn: rusqlite::Connection, durable: bool) -> Result<Self, TurnStoreError> {
        if durable {
            // Same pragmas as the dispatcher's store on the same file:
            // SQLite stores the journal mode per database, and both
            // connections must agree that it is WAL.
            let _mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(TurnStoreError::from)?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(TurnStoreError::from)?;
        }
        conn.execute_batch(SCHEMA).map_err(TurnStoreError::from)?;
        migrate_legacy_blob_table(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Appends `events` to `key`'s stream, in order, durable before it
    /// returns. The stream's sequence is the table's, so appends extend
    /// and never rewrite what a replay already read.
    pub fn append(&self, key: &str, events: &[TurnEvent]) -> Result<(), TurnStoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction().map_err(TurnStoreError::from)?;
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM turn_events WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .map_err(TurnStoreError::from)?;
        for (offset, event) in events.iter().enumerate() {
            let json = serde_json::to_string(event).map_err(|err| TurnStoreError::Corrupt {
                key: key.to_owned(),
                source: err,
            })?;
            tx.execute(
                "INSERT INTO turn_events (key, seq, event) VALUES (?1, ?2, ?3)",
                params![key, next + offset as i64, json],
            )
            .map_err(TurnStoreError::from)?;
        }
        tx.commit().map_err(TurnStoreError::from)?;
        Ok(())
    }

    /// `key`'s whole recorded stream, oldest first — the replay a fresh
    /// subscriber folds before the live feed.
    pub fn events(&self, key: &str) -> Result<Vec<TurnEvent>, TurnStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn.prepare("SELECT event FROM turn_events WHERE key = ?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(TurnStoreError::from)?;
            out.push(parse(key, &json)?);
        }
        Ok(out)
    }
}

/// Moves a legacy `turn_history` table (one JSON-array blob per key,
/// rewritten whole) into per-event rows, then drops it. A blob that no
/// longer parses is skipped with a warning: half a migration beats a
/// boot that cannot open its history at all. No-op when the table is
/// absent (every database this code created fresh).
fn migrate_legacy_blob_table(conn: &rusqlite::Connection) -> Result<(), TurnStoreError> {
    let legacy: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'turn_history'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .map_err(TurnStoreError::from)?;
    if !legacy {
        return Ok(());
    }
    let mut stmt = conn.prepare("SELECT key, events FROM turn_history")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut migrated = 0usize;
    for row in rows {
        let (key, blob) = row.map_err(TurnStoreError::from)?;
        let events: Vec<Value> = match serde_json::from_str(&blob) {
            Ok(events) => events,
            Err(err) => {
                eprintln!(
                    "[claw] warning: legacy transcript for {key} is unreadable, dropped: {err}"
                );
                continue;
            }
        };
        for (seq, event) in events.into_iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO turn_events (key, seq, event) VALUES (?1, ?2, ?3)",
                params![key, seq as i64, event.to_string()],
            )
            .map_err(TurnStoreError::from)?;
        }
        migrated += 1;
    }
    drop(stmt);
    conn.execute("DROP TABLE turn_history", [])
        .map_err(TurnStoreError::from)?;
    if migrated > 0 {
        eprintln!("[claw] migrated {migrated} legacy transcript(s) to the append-only table");
    }
    Ok(())
}

/// Parses one stored event. The JSON value step keeps the error message
/// about *what* the row holds, not just that serde choked.
fn parse(key: &str, json: &str) -> Result<TurnEvent, TurnStoreError> {
    let value: Value = serde_json::from_str(json).map_err(|source| TurnStoreError::Corrupt {
        key: key.to_owned(),
        source,
    })?;
    serde_json::from_value(value).map_err(|source| TurnStoreError::Corrupt {
        key: key.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(text: &str) -> TurnEvent {
        TurnEvent::TextDelta {
            text: text.to_owned(),
        }
    }

    #[test]
    fn an_appended_history_round_trips_through_events() {
        let store = TurnStore::in_memory().expect("store opens");
        assert_eq!(store.events("interactive:web").expect("reads"), vec![]);

        store
            .append(
                "interactive:web",
                &[
                    TurnEvent::UserMessage {
                        text: "hello".into(),
                        images: Vec::new(),
                    },
                    text_delta("hi"),
                ],
            )
            .expect("appends");
        store
            .append(
                "interactive:web",
                &[TurnEvent::TurnDone {
                    reply: Some("hi".into()),
                    outcome: "handled".into(),
                }],
            )
            .expect("appends again");
        assert_eq!(
            store.events("interactive:web").expect("reads"),
            vec![
                TurnEvent::UserMessage {
                    text: "hello".into(),
                    images: Vec::new()
                },
                text_delta("hi"),
                TurnEvent::TurnDone {
                    reply: Some("hi".into()),
                    outcome: "handled".into(),
                },
            ],
            "appends extend the stream in order"
        );
    }

    #[test]
    fn an_empty_append_is_quiet() {
        let store = TurnStore::in_memory().expect("store opens");
        store
            .append("interactive:web", &[])
            .expect("an empty append is a no-op");
        assert_eq!(store.events("interactive:web").expect("reads"), vec![]);
    }

    #[test]
    fn keys_are_separate_streams() {
        let store = TurnStore::in_memory().expect("store opens");
        store.append("task:2", &[text_delta("b")]).expect("appends");
        store
            .append("interactive:web", &[text_delta("a")])
            .expect("appends");
        assert_eq!(
            store.events("task:2").expect("reads"),
            vec![text_delta("b")],
            "one key's stream never leaks into another's"
        );
        assert_eq!(store.events("interactive:web").expect("reads").len(), 1);
    }

    #[test]
    fn a_corrupt_event_row_is_loud_not_empty() {
        let store = TurnStore::in_memory().expect("store opens");
        {
            let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
            conn.execute(
                "INSERT INTO turn_events (key, seq, event) VALUES ('interactive:web', 0, 'garbage')",
                [],
            )
            .expect("writes raw");
        }
        let err = store
            .events("interactive:web")
            .expect_err("corruption is loud");
        assert!(err.to_string().contains("interactive:web"), "{err}");
    }

    /// The one-time migration: a pre-window blob table opens into the
    /// append-only shape, its rows gone.
    #[test]
    fn a_legacy_blob_table_migrates_into_event_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dispatch.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("opens");
            conn.execute_batch(
                "CREATE TABLE turn_history (
                     key        TEXT PRIMARY KEY,
                     events     TEXT NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 INSERT INTO turn_history VALUES ('interactive:web',
                     '[{\"type\":\"text_delta\",\"text\":\"old days\"}]', 1);
                 INSERT INTO turn_history VALUES ('interactive:broke', 'not json', 2);",
            )
            .expect("legacy table");
        }
        let store = TurnStore::open(&path).expect("opens and migrates");
        assert_eq!(
            store.events("interactive:web").expect("reads"),
            vec![text_delta("old days")],
            "the readable blob migrated"
        );
        assert_eq!(store.events("interactive:broke").expect("reads"), vec![]);
        {
            let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
            let legacy: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'turn_history'",
                    [],
                    |row| row.get(0),
                )
                .expect("reads");
            assert_eq!(legacy, 0, "the legacy table is gone");
        }
        // The migrated rows keep appending after the migration.
        store
            .append("interactive:web", &[text_delta("new days")])
            .expect("appends");
        assert_eq!(store.events("interactive:web").expect("reads").len(), 2);
    }
}
