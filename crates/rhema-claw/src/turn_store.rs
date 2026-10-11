//! The turn-history table: one durable row per resident key, holding
//! the key's recorded [`TurnEvent`]s as one JSON blob, in the same
//! database file the dispatcher's
//! [`SqliteStore`](rhema_rsd::SqliteStore) and the other claw tables
//! use.
//!
//! The event hub keeps the live record in memory and flushes it here on
//! every turn's end marker, so a stream's history survives a daemon
//! restart — the session stash keeps the *conversation* (what the model
//! sees); this table keeps the *transcript* (what a viewer saw, tool
//! rows and thinking traces included). Writes are one UPSERT per turn,
//! never per delta; the cap lives hub-side, so what lands here is
//! already bounded.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde_json::Value;

use crate::hub::TurnEvent;

/// The turn-history table.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS turn_history (
    key        TEXT PRIMARY KEY,
    events     TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// Why a turn-history operation failed. A row whose blob does not parse
/// is its own variant: a transcript that cannot replay must say so, not
/// silently read as "no history".
#[derive(Debug, thiserror::Error)]
pub enum TurnStoreError {
    /// The SQLite backend failed.
    #[error("turn history backend: {0}")]
    Backend(#[from] rusqlite::Error),
    /// A stored history blob is not a JSON array of turn events.
    #[error("turn history for {key} does not parse: {source}")]
    Corrupt {
        /// The key whose blob failed.
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
    /// at `path` — the same file the dispatcher's store holds. WAL keeps
    /// the connections from blocking each other's readers; the busy
    /// timeout rides out the other's write locks.
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Replaces `key`'s whole recorded history with `events` — the hub
    /// holds the live record and flushes it whole, so the write is one
    /// UPSERT, durable before it returns.
    pub fn save(&self, key: &str, events: &[TurnEvent]) -> Result<(), TurnStoreError> {
        let blob = serde_json::to_string(events).map_err(|err| TurnStoreError::Corrupt {
            key: key.to_owned(),
            source: err,
        })?;
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO turn_history (key, events, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET events = ?2, updated_at = ?3",
            params![key, blob, now_millis() as i64],
        )?;
        Ok(())
    }

    /// `key`'s recorded history, oldest first; `None` when the key has
    /// none.
    pub fn load(&self, key: &str) -> Result<Option<Vec<TurnEvent>>, TurnStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let blob: Option<String> = conn
            .query_row(
                "SELECT events FROM turn_history WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        blob.map(|blob| parse(key, &blob)).transpose()
    }

    /// Every key with a recorded history and its events — the boot seed
    /// the hub replays from.
    pub fn all(&self) -> Result<Vec<(String, Vec<TurnEvent>)>, TurnStoreError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt = conn.prepare("SELECT key, events FROM turn_history ORDER BY key")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, blob) = row.map_err(TurnStoreError::from)?;
            let events = parse(&key, &blob)?;
            out.push((key, events));
        }
        Ok(out)
    }
}

/// Parses one stored blob into turn events. The JSON value step keeps
/// the error message about *what* the blob holds, not just that serde
/// choked.
fn parse(key: &str, blob: &str) -> Result<Vec<TurnEvent>, TurnStoreError> {
    let value: Value = serde_json::from_str(blob).map_err(|source| TurnStoreError::Corrupt {
        key: key.to_owned(),
        source,
    })?;
    serde_json::from_value(value).map_err(|source| TurnStoreError::Corrupt {
        key: key.to_owned(),
        source,
    })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
    fn a_saved_history_round_trips_through_load() {
        let store = TurnStore::in_memory().expect("store opens");
        assert_eq!(store.load("interactive:web").expect("reads"), None);

        let history = vec![
            TurnEvent::UserMessage {
                text: "hello".into(),
            },
            text_delta("hi"),
            TurnEvent::TurnDone {
                reply: Some("hi".into()),
                outcome: "handled".into(),
            },
        ];
        store.save("interactive:web", &history).expect("saves");
        assert_eq!(
            store.load("interactive:web").expect("reads"),
            Some(history),
            "every event, in order, survives the table"
        );
    }

    #[test]
    fn a_save_replaces_the_whole_record() {
        let store = TurnStore::in_memory().expect("store opens");
        store
            .save("interactive:web", &[text_delta("old")])
            .expect("saves");
        store
            .save("interactive:web", &[text_delta("new"), text_delta("er")])
            .expect("saves again");
        assert_eq!(
            store.load("interactive:web").expect("reads"),
            Some(vec![text_delta("new"), text_delta("er")]),
            "the flush is whole-record: no stale tail"
        );
    }

    #[test]
    fn keys_are_separate_and_all_lists_them_sorted() {
        let store = TurnStore::in_memory().expect("store opens");
        store.save("task:2", &[text_delta("b")]).expect("saves");
        store
            .save("interactive:web", &[text_delta("a")])
            .expect("saves");
        let all = store.all().expect("reads");
        assert_eq!(
            all.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec!["interactive:web", "task:2"],
            "deterministic enumeration, per key"
        );
        assert_eq!(all[1].1, vec![text_delta("b")]);
    }

    #[test]
    fn a_corrupt_blob_is_loud_not_empty() {
        let store = TurnStore::in_memory().expect("store opens");
        {
            let conn = store.conn.lock().unwrap_or_else(PoisonError::into_inner);
            conn.execute(
                "INSERT INTO turn_history (key, events, updated_at) VALUES ('interactive:web', 'garbage', 0)",
                [],
            )
            .expect("writes raw");
        }
        let err = store
            .load("interactive:web")
            .expect_err("corruption is loud");
        assert!(err.to_string().contains("interactive:web"), "{err}");
    }
}
