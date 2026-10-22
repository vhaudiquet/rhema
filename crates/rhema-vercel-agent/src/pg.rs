//! The sessions, in Postgres.
//!
//! [`PgStore`] implements the one [`Store`] contract — the same
//! revision-guarded records the Durable Object keeps in its SQLite —
//! over two tables, plus the meta table the per-session config and
//! the seed marker live in. The rows are JSON, exactly the shapes
//! `serde` already produces for the DO, so a record written by one
//! frontend reads like one written by the other.
//!
//! The client is the sync `postgres` driver, on purpose: the Store
//! trait is synchronous — the engine's commit hook calls it from
//! inside the turn — and a sync round trip to the database (a local
//! pool in production, a container here) is milliseconds against a
//! turn dominated by model streams. Making the trait async would fork
//! the contract every store implements; the honest note instead: a
//! turn's checkpoint briefly parks its worker thread, the way the ACP
//! adapter parks one waiting for the editor.
#![forbid(unsafe_code)]

use rhema_rsd::{KeyRecord, ResourceKey, Revision, Store, StoreError};
use std::sync::{Arc, Mutex};

/// The store over one Postgres database: a pooled connection guarded
/// by a mutex, the schema created on open. Cloneable — every clone
/// shares the one connection, so a commit hook can hold its own
/// handle.
#[derive(Clone)]
pub struct PgStore {
    client: Arc<Mutex<postgres::Client>>,
}

/// A postgres error, rendered with its source chain — `Display`
/// alone says `db error` and stops, and the DB's own words are what
/// an operator needs.
fn render(err: postgres::Error) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl PgStore {
    /// Opens the connection `url` points at and creates the schema.
    /// TLS engages when the URL asks for it (`sslmode=require` and
    /// friends — the Neon default), through the system's native TLS.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        let connector = postgres_native_tls::MakeTlsConnector::new(
            native_tls::TlsConnector::new()
                .map_err(|err| StoreError::Backend(format!("tls: {err}")))?,
        );
        let client = postgres::Client::connect(url, connector)
            .map_err(|err| StoreError::Backend(format!("connect: {err}")))?;
        let store = Self {
            client: Arc::new(Mutex::new(client)),
        };
        store.migrate()?;
        Ok(store)
    }

    /// The schema, created if absent.
    fn migrate(&self) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS records (
                     key TEXT PRIMARY KEY,
                     revision BIGINT NOT NULL,
                     data JSONB NOT NULL);
                 CREATE TABLE IF NOT EXISTS meta (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL);",
            )
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    /// The guarded connection.
    fn lock(&self) -> std::sync::MutexGuard<'_, postgres::Client> {
        self.client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The stored meta value, `None` when unset — the config slot and
    /// the seed marker live here.
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_opt("SELECT value FROM meta WHERE key = $1", &[&key])
            .map_err(|err| StoreError::Backend(render(err)))?;
        Ok(row.map(|row| row.get(0)))
    }

    /// Writes one meta value, replacing any previous.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "INSERT INTO meta (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                &[&key, &value],
            )
            .map_err(|err| StoreError::Backend(render(err)))?;
        Ok(())
    }
}

impl Store for PgStore {
    fn load(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_opt("SELECT data FROM records WHERE key = $1", &[&key.as_str()])
            .map_err(|err| StoreError::Backend(render(err)))?;
        match row {
            None => Ok(None),
            Some(row) => {
                let data: serde_json::Value = row.get("data");
                Ok(Some(serde_json::from_value(data)?))
            }
        }
    }

    fn save(
        &self,
        record: KeyRecord,
        expected_revision: Option<Revision>,
    ) -> Result<(), StoreError> {
        let data = serde_json::to_value(&record)?;
        let key = record.key.as_str().to_owned();
        let revision = record.revision as i64;
        let mut client = self.lock();
        let written = match expected_revision {
            // The key must not exist yet; a conflict is a rejection.
            None => client
                .execute(
                    "INSERT INTO records (key, revision, data) VALUES ($1, $2, $3)
                     ON CONFLICT (key) DO NOTHING",
                    &[&key, &revision, &data],
                )
                .map_err(|err| StoreError::Backend(render(err)))?,
            Some(expected) => client
                .execute(
                    "UPDATE records SET revision = $2, data = $3
                     WHERE key = $1 AND revision = $4",
                    &[&key, &revision, &data, &(expected as i64)],
                )
                .map_err(|err| StoreError::Backend(render(err)))?,
        };
        if written == 0 {
            // The stored revision, for the message — queried on the
            // connection this method already holds (the lock is not
            // reentrant; a helper here would deadlock).
            let actual = client
                .query_opt("SELECT revision FROM records WHERE key = $1", &[&key])
                .map_err(|err| StoreError::Backend(render(err)))?
                .map(|row| row.get::<_, i64>(0) as Revision)
                .unwrap_or(0);
            return Err(StoreError::RevisionMismatch {
                expected: expected_revision.unwrap_or(0),
                actual,
            });
        }
        Ok(())
    }

    fn keys(&self) -> Result<Vec<ResourceKey>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query("SELECT key FROM records ORDER BY key", &[])
            .map_err(|err| StoreError::Backend(render(err)))?;
        Ok(rows
            .iter()
            .map(|row| ResourceKey::new(row.get::<_, String>(0)))
            .collect())
    }
}
