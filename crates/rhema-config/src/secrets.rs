//! The secret-store seam: where provider keys live when they are not
//! in `config.yaml`.
//!
//! The config file stores, per provider instance, either the key
//! itself (the historical shape, still the fallback) or an opaque
//! `secret_ref` naming a store entry. The [`SecretStore`] trait is the
//! seam between the two: get, set, and delete of one opaque value per
//! id, nothing more, no accounts, no metadata, no model-facing
//! surface.
//!
//! The backend is [`EncryptedFileSecretStore`]: one age ciphertext
//! under the config root and one generated X25519 identity under the
//! state root. It needs no OS keyring and no user input, so it behaves
//! the same on a desktop, over SSH, and in a container. A ciphertext
//! copied on its own carries no readable credential. [`VolatileSecretStore`]
//! serves tests. A frontend asks its [`crate::Home`] for a store.
//!
//! [`KeyringSecretReader`] is the one-way importer for machines that
//! used the old OS keyring: it reads entries, never writes or deletes,
//! so the migration cannot resurrect the locked-store failures in the
//! write path. A local start copies the config's missing `secret_ref`s
//! once; an SSH start skips it unless `RHEMA_IMPORT_KEYRING` forces
//! the attempt.
//!
//! Availability is per operation, never assumed at startup. A missing
//! identity beside an existing ciphertext is reported as
//! [`SecretError::MissingKey`], never treated as an empty store, so a
//! write cannot replace the ciphertext it cannot read.

#![forbid(unsafe_code)]

mod file;

pub use file::EncryptedFileSecretStore;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The reference frontend's old OS keyring service name: the namespace
/// the pre-file-store backend wrote under. The read-only importer reads
/// it; nothing writes there any more. A relocated frontend's name comes
/// from its [`crate::Home`].
pub const RHEMA_SERVICE: &str = "rhema";

/// Why a secret-store operation failed. Resolution treats a read error
/// as "not in the store"; the variants exist for callers that want to
/// say why a key could not be placed.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The platform provides no reachable credential store.
    #[error("no reachable secret store on this platform")]
    NoStore,
    /// The ciphertext exists but its identity does not. Writing a new
    /// identity here would destroy the old ciphertext, so callers must
    /// surface this instead.
    #[error(
        "the secret store is missing its key: {key} (restore it, or delete {store} to start over)"
    )]
    MissingKey {
        /// The key file the ciphertext expected.
        key: String,
        /// The ciphertext that needs that key.
        store: String,
    },
    /// The store refused or fumbled the operation.
    #[error("secret store: {0}")]
    Backend(String),
}

/// The secret-store contract: put, read, and remove one opaque value
/// per id. Sync and `Send + Sync`, like the crate's other seams,
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

    /// Whether the store answers at all, probed honestly. A file store
    /// reports a ciphertext it cannot decrypt, a missing identity, and
    /// an unreadable file. Frontends run this before blaming a missing
    /// key on the config. Stores that cannot fail answer yes; the
    /// default serves them.
    fn reachable(&self) -> bool {
        true
    }
}

/// The read-only side of the old OS keyring: get, never set or
/// delete. Kept separate from [`SecretStore`] so a migration cannot
/// write to the legacy backend by accident.
pub trait LegacySecretStore: Send + Sync {
    /// The value stored under `id`, `None` when absent. An unreachable
    /// keyring is an error; the importer treats every error as "cannot
    /// migrate this id" and leaves the config alone.
    fn get(&self, id: &str) -> Result<Option<String>, SecretError>;
}

/// Reads the OS keyring the pre-file-store versions wrote to. The
/// reader only calls `get_password`; it never probes by writing and
/// never deletes, so a locked or unreachable keyring is a quiet skip,
/// not a prompt loop. [`KeyringSecretReader::for_home`] disables it in
/// an SSH session unless `RHEMA_IMPORT_KEYRING` says otherwise; the
/// first error disables the instance for the rest of the start.
#[derive(Debug)]
pub struct KeyringSecretReader {
    service: std::borrow::Cow<'static, str>,
    enabled: bool,
    available: AtomicBool,
}

impl KeyringSecretReader {
    /// The reader for one frontend home: its old service name from
    /// [`crate::Home::legacy_keyring_service`], enabled only for a
    /// local session or an explicit `RHEMA_IMPORT_KEYRING` override.
    pub fn for_home(home: &crate::Home) -> Self {
        let force = std::env::var("RHEMA_IMPORT_KEYRING").ok();
        Self {
            service: home.legacy_keyring_service().into(),
            enabled: keyring_import_allowed(force.as_deref(), remote_session()),
            available: AtomicBool::new(true),
        }
    }

    /// A reader over an explicit service name, for a frontend that
    /// used a namespace of its own rather than [`crate::Home`]. The
    /// local/override gate is the same as [`Self::for_home`].
    pub fn named(service: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        let force = std::env::var("RHEMA_IMPORT_KEYRING").ok();
        Self {
            service: service.into(),
            enabled: keyring_import_allowed(force.as_deref(), remote_session()),
            available: AtomicBool::new(true),
        }
    }

    /// A reader that answers [`SecretError::NoStore`] without touching
    /// the OS: tests, and a frontend that opted out of the import.
    pub fn disabled() -> Self {
        Self {
            service: RHEMA_SERVICE.into(),
            enabled: false,
            available: AtomicBool::new(true),
        }
    }
}

impl LegacySecretStore for KeyringSecretReader {
    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        if !self.enabled || !self.available.load(Ordering::Relaxed) {
            return Err(SecretError::NoStore);
        }
        let entry = keyring::v1::Entry::new(self.service.as_ref(), id).map_err(|err| {
            self.available.store(false, Ordering::Relaxed);
            translate(err)
        })?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(err) => {
                self.available.store(false, Ordering::Relaxed);
                Err(translate(err))
            }
        }
    }
}

/// The keyring crate's errors, cut to the two this crate's callers can
/// act on.
fn translate(err: keyring::v1::Error) -> SecretError {
    match err {
        keyring::v1::Error::NoStorageAccess(_) => SecretError::NoStore,
        other => SecretError::Backend(other.to_string()),
    }
}

/// Whether this process looks like an SSH session: OpenSSH sets at
/// least one of these on every platform. The keyring importer leaves
/// such a session alone by default, because the OS keyring is exactly
/// what does not answer there.
pub fn remote_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
}

/// The importer's gate: `RHEMA_IMPORT_KEYRING` forces or refuses the
/// attempt; absent, an SSH session refuses it and a local one allows
/// it.
fn keyring_import_allowed(force: Option<&str>, remote: bool) -> bool {
    match force.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value)
            if ["0", "false", "no", "off"]
                .iter()
                .any(|off| value.eq_ignore_ascii_case(off)) =>
        {
            false
        }
        Some(_) => true,
        None => !remote,
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

/// A fresh secret-store id: wall-clock millis, the clock's sub-milli
/// nanos, and a process counter in hex. The id is a handle, not a
/// secret, it only has to be unique within one machine's config, and
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
    fn the_keyring_import_gate_respects_ssh_and_override() {
        assert!(!keyring_import_allowed(None, true));
        assert!(keyring_import_allowed(None, false));
        assert!(keyring_import_allowed(Some("1"), true));
        assert!(!keyring_import_allowed(Some("0"), false));
        assert!(!keyring_import_allowed(Some("off"), false));
        assert!(!keyring_import_allowed(Some(""), true));
    }

    #[test]
    fn a_disabled_keyring_reader_never_touches_the_os() {
        assert!(matches!(
            KeyringSecretReader::disabled().get("any-id"),
            Err(SecretError::NoStore)
        ));
    }

    #[test]
    fn fresh_ids_differ_and_fit_the_entry_shape() {
        let first = fresh_id();
        let second = fresh_id();
        assert_ne!(first, second);
        for id in [first, second] {
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "ids stay store-safe: {id}"
            );
        }
    }
}
