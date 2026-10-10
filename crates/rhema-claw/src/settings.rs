//! Runtime settings the daemon persists: the key-value table behind the
//! deployment state that outlives a restart.
//!
//! The active provider/model pair lives here, not in `claw.yaml` — the
//! config file seeds it, the API changes it, and this store keeps it.
//! Same store shape as the crate's other tables: a second connection to
//! the shared SQLite file, WAL, busy timeout.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::OptionalExtension;
use rusqlite::params;

/// Why a settings operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The backend failed to read or write.
    #[error("settings store failed: {0}")]
    Backend(String),
}

/// The `settings` key-value table over the claw's shared database.
pub struct SettingsStore {
    conn: Mutex<rusqlite::Connection>,
}

impl SettingsStore {
    /// Opens (creating if needed) the settings table on the database at
    /// `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Self::init(conn)
    }

    /// Opens the settings table over an in-memory database.
    pub fn in_memory() -> Result<Self, SettingsError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, SettingsError> {
        let conn = conn;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| SettingsError::Backend(err.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The value stored for `key`, when one was set.
    pub fn get(&self, key: &str) -> Result<Option<String>, SettingsError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Ok(value)
    }

    /// Sets `key` to `value`, replacing any previous value.
    pub fn set(&self, key: &str, value: &str) -> Result<(), SettingsError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Ok(())
    }

    /// Removes `key` from the store; a no-op when it was absent.
    pub fn delete(&self, key: &str) -> Result<(), SettingsError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.execute("DELETE FROM settings WHERE key = ?1", params![key])
            .map_err(|err| SettingsError::Backend(err.to_string()))?;
        Ok(())
    }
}

/// The persisted settings keys: the active backend pair.
pub const ACTIVE_PROVIDER_KEY: &str = "active_provider";
/// The persisted active model.
pub const ACTIVE_MODEL_KEY: &str = "active_model";
