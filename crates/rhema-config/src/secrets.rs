//! The secret-store seam: where provider keys live when they are not
//! in `config.yaml`.
//!
//! The config file stores, per provider instance, either the key
//! itself (the historical shape, still the fallback) or an opaque
//! `secret_ref` naming a keychain entry. The [`SecretStore`] trait is
//! the seam between the two: get, set, and delete of one opaque value
//! per id, nothing more — no accounts, no metadata, no model-facing
//! surface. The OS keychain backend ([`KeyringSecretStore`], the
//! `keyring` crate over macOS Keychain, Windows Credential Manager,
//! and the Linux Secret Service) is the point; [`VolatileSecretStore`]
//! serves tests.
//!
//! Availability is per operation, never assumed at startup: a machine
//! with no keychain (a container, a headless box) reports errors from
//! every call, and the resolution tiers in [`crate::Profile`] treat
//! that as "not in the store", falling through to the literal. A save
//! that could not reach the store keeps the key in the file — the
//! pre-keychain behavior, never a lost credential.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The keychain service name every entry of this crate lives under.
const SERVICE: &str = "rhema";

/// Why a secret-store operation failed. Resolution treats every error
/// as "not in the store"; the variants exist for callers that want to
/// say *why* a key could not be placed.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The platform provides no reachable credential store: no keychain
    /// session, no Secret Service on the bus, no backend for this
    /// target at all.
    #[error("no reachable credential store on this platform")]
    NoStore,
    /// The store refused or fumbled the operation.
    #[error("credential store: {0}")]
    Backend(String),
}

/// The secret-store contract: put, read, and remove one opaque value
/// per id. Sync and `Send + Sync`, like the crate's other seams —
/// operations are short local calls, serialized by the implementation.
pub trait SecretStore: Send + Sync {
    /// The value stored under `id`, `None` when absent. A store that
    /// cannot be reached is an error, not a silent `None`.
    fn get(&self, id: &str) -> Result<Option<String>, SecretError>;

    /// Stores `value` under `id`, replacing any previous one.
    fn set(&self, id: &str, value: &str) -> Result<(), SecretError>;

    /// Removes `id`, reporting whether it was there. Absent is `Ok`,
    /// not an error: deletion is idempotent.
    fn delete(&self, id: &str) -> Result<bool, SecretError>;

    /// Whether the store answers at all, probed honestly: one write of
    /// a throwaway entry and its removal. A read of a fresh id cannot
    /// tell a locked keyring from an absent key, so the write is what
    /// gets probed — a locked or missing store refuses it. Frontends
    /// run this before blaming a missing key on the config. Stores
    /// that cannot fail answer yes; the default serves them.
    fn reachable(&self) -> bool {
        true
    }
}

/// The OS keychain backend: every id is an entry under the `rhema`
/// service, holding the key as its secret. The struct is stateless —
/// availability is decided per operation by the platform, so one value
/// serves a whole process.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringSecretStore;

impl KeyringSecretStore {
    /// The process-wide store.
    pub fn new() -> Self {
        Self
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        let entry = open(id)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(err) => Err(translate(err)),
        }
    }

    fn set(&self, id: &str, value: &str) -> Result<(), SecretError> {
        open(id)?.set_password(value).map_err(translate)
    }

    fn delete(&self, id: &str) -> Result<bool, SecretError> {
        let entry = open(id)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::v1::Error::NoEntry) => Ok(false),
            Err(err) => Err(translate(err)),
        }
    }

    fn reachable(&self) -> bool {
        // The write decides: a locked collection and an unreachable
        // store both refuse it. A store that took the write but
        // fumbled the delete reads as unreachable, leaving the probe
        // entry behind — its id is unique to this call and its value
        // is not a secret.
        let id = fresh_id();
        if self.set(&id, "reachability probe").is_err() {
            return false;
        }
        self.delete(&id).unwrap_or_default()
    }
}

/// Opens the id's entry under the `rhema` service.
fn open(id: &str) -> Result<keyring::v1::Entry, SecretError> {
    keyring::v1::Entry::new(SERVICE, id).map_err(translate)
}

/// The keyring crate's errors, cut to the two this crate's callers can
/// act on.
fn translate(err: keyring::v1::Error) -> SecretError {
    match err {
        keyring::v1::Error::NoStorageAccess(_) => SecretError::NoStore,
        other => SecretError::Backend(other.to_string()),
    }
}

/// The in-process store: a plain map, for tests and nothing else.
#[derive(Debug, Default)]
pub struct VolatileSecretStore(Mutex<BTreeMap<String, String>>);

impl VolatileSecretStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for VolatileSecretStore {
    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        Ok(self.0.lock().expect("volatile store").get(id).cloned())
    }

    fn set(&self, id: &str, value: &str) -> Result<(), SecretError> {
        self.0
            .lock()
            .expect("volatile store")
            .insert(id.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<bool, SecretError> {
        Ok(self.0.lock().expect("volatile store").remove(id).is_some())
    }
}

/// A fresh keychain-entry id: wall-clock millis, the clock's sub-milli
/// nanos, and a process counter in hex. The id is a handle, not a
/// secret — it only has to be unique within one machine's config, and
/// stable once stored in the file.
pub fn fresh_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:013x}-{:08x}-{n:04x}",
        now.as_millis(),
        now.subsec_nanos()
    )
}

/// The replayable [`SecretStore`] behavioral suite, identity's
/// `store_contract` pattern: plain assertion bodies taking any store,
/// replayed by every backend's tests, `run_all` running the set in
/// order.
pub mod contract {
    use super::{SecretStore, fresh_id};

    /// `set` then `get` returns the value, byte for byte.
    pub fn set_then_get_round_trips(store: &dyn SecretStore) {
        let id = fresh_id();
        store.set(&id, "sk-contract-1").unwrap();
        assert_eq!(store.get(&id).unwrap().as_deref(), Some("sk-contract-1"));
    }

    /// `get` on an id the store never held is `None`, not an error.
    pub fn get_missing_returns_none(store: &dyn SecretStore) {
        assert_eq!(store.get(&fresh_id()).unwrap(), None);
    }

    /// `set` overwrites: the id holds exactly one value.
    pub fn set_overwrites(store: &dyn SecretStore) {
        let id = fresh_id();
        store.set(&id, "first").unwrap();
        store.set(&id, "second").unwrap();
        assert_eq!(store.get(&id).unwrap().as_deref(), Some("second"));
    }

    /// `delete` removes, reports what it did, and stays idempotent.
    pub fn delete_removes_and_reports_absent(store: &dyn SecretStore) {
        let id = fresh_id();
        store.set(&id, "gone soon").unwrap();
        assert!(store.delete(&id).unwrap(), "the first delete removes");
        assert!(!store.delete(&id).unwrap(), "the second reports absent");
        assert_eq!(store.get(&id).unwrap(), None);
    }

    /// The store answers before any behavior is replayed against it.
    pub fn is_reachable(store: &dyn SecretStore) {
        assert!(store.reachable(), "the store answers");
    }

    /// The whole suite, in order.
    pub fn run_all(store: &dyn SecretStore) {
        is_reachable(store);
        set_then_get_round_trips(store);
        get_missing_returns_none(store);
        set_overwrites(store);
        delete_removes_and_reports_absent(store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volatile_store_replays_the_contract() {
        contract::run_all(&VolatileSecretStore::new());
    }

    #[test]
    fn fresh_ids_differ_and_fit_the_entry_shape() {
        let first = fresh_id();
        let second = fresh_id();
        assert_ne!(first, second);
        for id in [first, second] {
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "ids stay keyring-safe: {id}"
            );
        }
    }
}
