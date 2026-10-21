//! The DO's SQLite storage as the persistence layer.
//!
//! Sessions checkpoint as [`KeyRecord`]s through the
//! [`rhema_rsd::Store`] contract — one row per session key, the
//! revision column the compare-and-swap token — so the DO gets the
//! same revision-guarded persistence the dispatcher spec'd, not an
//! ad-hoc table. The workspace's seeded flag lives beside them in a
//! meta table. One row, one writer: the DO instance per session id is
//! the single writer, and the revision check still holds the line
//! against a split brain during eviction. `SqlStorage` calls are
//! synchronous (no await points), so a read-modify-write between two
//! of them cannot interleave with another task.
#![forbid(unsafe_code)]

use rhema_rsd::{KeyRecord, ResourceKey, Revision, Store, StoreError};
use worker::SqlStorage;

/// The storage-backed store. Cloneable — the inner handle is a JS
/// object the runtime keeps alive — and `Send + Sync` by the runtime's
/// own guarantee for storage handles.
#[derive(Clone)]
pub struct DoStore {
    sql: SqlStorage,
}

/// The one session key this DO persists.
pub fn session_key(session_id: &str) -> ResourceKey {
    ResourceKey::new(format!("coding-agent/{session_id}"))
}

/// Milliseconds since the Unix epoch, from the host clock — the
/// timestamps a fresh [`KeyRecord`] carries.
pub fn now_millis() -> u64 {
    worker::js_sys::Date::now() as u64
}

/// The record a session starts from: fresh, with the session absent
/// until the first checkpoint lands.
pub fn fresh_record(key: ResourceKey) -> KeyRecord {
    KeyRecord::fresh(key, now_millis())
}

impl DoStore {
    /// Opens (and creates) the schema on the DO's database.
    pub fn new(sql: SqlStorage) -> Result<Self, StoreError> {
        let store = Self { sql };
        store.run(
            "CREATE TABLE IF NOT EXISTS records (
                key TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                data BLOB NOT NULL
            )",
            None,
        )?;
        store.run(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            None,
        )?;
        Ok(store)
    }

    /// Runs one statement, mapping worker errors below the seam.
    fn run(
        &self,
        query: &str,
        bindings: Option<Vec<worker::SqlStorageValue>>,
    ) -> Result<worker::SqlCursor, StoreError> {
        self.sql
            .exec(query, bindings)
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    /// The stored value of a meta key, if any.
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        let cursor = self.run(
            "SELECT value FROM meta WHERE key = ?1",
            Some(vec![worker::SqlStorageValue::String(key.to_owned())]),
        )?;
        Ok(cursor
            .next::<MetaRow>()
            .filter_map(Result::ok)
            .map(|row| row.value)
            .next())
    }

    /// Upserts a meta key.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            Some(vec![
                worker::SqlStorageValue::String(key.to_owned()),
                worker::SqlStorageValue::String(value.to_owned()),
            ]),
        )?;
        Ok(())
    }

    /// The checkpointed record for a key, with its revision.
    pub fn load_checked(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, StoreError> {
        let cursor = self.run(
            "SELECT data FROM records WHERE key = ?1",
            Some(vec![worker::SqlStorageValue::String(
                key.as_str().to_owned(),
            )]),
        )?;
        let row = cursor
            .next::<DataRow>()
            .filter_map(Result::ok)
            .map(|row| row.data)
            .next();
        match row {
            Some(data) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    /// The stored revision of a key, for the conflict diagnostics.
    fn stored_revision(&self, key: &ResourceKey) -> Result<Option<Revision>, StoreError> {
        let cursor = self.run(
            "SELECT revision FROM records WHERE key = ?1",
            Some(vec![worker::SqlStorageValue::String(
                key.as_str().to_owned(),
            )]),
        )?;
        Ok(cursor
            .next::<RevisionRow>()
            .filter_map(Result::ok)
            .map(|row| row.revision)
            .next())
    }
}

#[derive(serde::Deserialize)]
struct DataRow {
    data: String,
}

#[derive(serde::Deserialize)]
struct RevisionRow {
    revision: Revision,
}

#[derive(serde::Deserialize)]
struct MetaRow {
    value: String,
}

impl Store for DoStore {
    fn load(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, StoreError> {
        self.load_checked(key)
    }

    fn save(
        &self,
        record: KeyRecord,
        expected_revision: Option<Revision>,
    ) -> Result<(), StoreError> {
        let data = serde_json::to_string(&record)?;
        let written = match expected_revision {
            // The key must not exist yet; a conflict is a rejection.
            None => {
                let cursor = self.run(
                    "INSERT INTO records (key, revision, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(key) DO NOTHING",
                    Some(vec![
                        worker::SqlStorageValue::String(record.key.as_str().to_owned()),
                        worker::SqlStorageValue::Integer(record.revision as i64),
                        worker::SqlStorageValue::String(data),
                    ]),
                )?;
                cursor.rows_written()
            }
            Some(expected) => {
                let cursor = self.run(
                    "UPDATE records SET revision = ?2, data = ?3
                     WHERE key = ?1 AND revision = ?4",
                    Some(vec![
                        worker::SqlStorageValue::String(record.key.as_str().to_owned()),
                        worker::SqlStorageValue::Integer(record.revision as i64),
                        worker::SqlStorageValue::String(data),
                        worker::SqlStorageValue::Integer(expected as i64),
                    ]),
                )?;
                cursor.rows_written()
            }
        };
        if written == 0 {
            let actual = self.stored_revision(&record.key)?.unwrap_or(0);
            return Err(StoreError::RevisionMismatch {
                expected: expected_revision.unwrap_or(0),
                actual,
            });
        }
        Ok(())
    }

    fn keys(&self) -> Result<Vec<ResourceKey>, StoreError> {
        let cursor = self.run("SELECT key FROM records ORDER BY key", None)?;
        Ok(cursor
            .next::<KeyRow>()
            .filter_map(Result::ok)
            .map(|row| ResourceKey::new(row.key))
            .collect())
    }
}

#[derive(serde::Deserialize)]
struct KeyRow {
    key: String,
}
