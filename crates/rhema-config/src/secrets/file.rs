//! The encrypted file backend: one age ciphertext holding the whole
//! id-to-value map, and one generated X25519 identity that opens it.
//!
//! The two live apart on purpose. The ciphertext sits under the config
//! root, the directory a frontend's dotfiles and backups pick up. The
//! identity sits under the state root, which they usually do not. A
//! ciphertext copied on its own is useless.
//!
//! Nothing here prompts. The identity is generated on the first write,
//! never derived from a passphrase, so a run over SSH is the same as a
//! run on the desktop. The trade is explicit: the store protects
//! against another user, an accidental read, and a config directory
//! that travels without the state directory. It does not protect
//! against a process running as the same user, which can read the key
//! file, and neither did the keyring.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use age::secrecy::ExposeSecret as _;

use super::{SecretError, SecretStore, fresh_id};
use crate::privacy::{private_create, private_dir, private_write_atomic, tighten_file};

/// The on-disk JSON shape. The version leaves room for a future
/// format without guessing at an old file's meaning.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Document {
    version: u8,
    entries: BTreeMap<String, String>,
}

/// One decrypted store: the identity and the map it opens.
struct Loaded {
    identity: age::x25519::Identity,
    entries: BTreeMap<String, String>,
}

/// The encrypted file backend. `ciphertext` is the age file, `key` is
/// the generated identity, both written `0600`.
#[derive(Debug)]
pub struct EncryptedFileSecretStore {
    ciphertext: PathBuf,
    key: PathBuf,
    /// Serializes writers in this process. Cross-process exclusion is
    /// the sibling lock file, acquired for the read-modify-write of a
    /// write and released on drop.
    write_lock: Mutex<()>,
}

impl EncryptedFileSecretStore {
    /// A store over the two explicit paths. Neither is touched until
    /// the first operation.
    pub fn new(ciphertext: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
        Self {
            ciphertext: ciphertext.into(),
            key: key.into(),
            write_lock: Mutex::new(()),
        }
    }

    /// The store for one frontend home: ciphertext under the config
    /// root, identity under the state root.
    pub fn for_home(home: &crate::Home) -> Self {
        Self::new(home.secrets_file(), home.secrets_key())
    }

    /// Reads and decrypts the whole store. A missing ciphertext is an
    /// empty store, with an existing identity reused or a fresh one
    /// generated in memory. A ciphertext with no identity is an error,
    /// never a silent reset.
    fn load(&self) -> Result<Loaded, SecretError> {
        let identity = self.read_identity()?;
        tighten_file(&self.ciphertext);
        match std::fs::read(&self.ciphertext) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Loaded {
                identity: identity.unwrap_or_else(age::x25519::Identity::generate),
                entries: BTreeMap::new(),
            }),
            Err(err) => Err(backend("cannot read the secret store", err)),
            Ok(bytes) => {
                let identity = identity.ok_or_else(|| SecretError::MissingKey {
                    key: self.key.display().to_string(),
                    store: self.ciphertext.display().to_string(),
                })?;
                let decryptor = age::Decryptor::new(bytes.as_slice())
                    .map_err(|err| backend("the secret store is not an age file", err))?;
                let mut reader = decryptor
                    .decrypt(std::iter::once(&identity as &dyn age::Identity))
                    .map_err(|err| backend("cannot decrypt the secret store", err))?;
                let mut plaintext = Vec::new();
                reader
                    .read_to_end(&mut plaintext)
                    .map_err(|err| backend("cannot read the secret store", err))?;
                let document: Document = serde_json::from_slice(&plaintext)
                    .map_err(|err| backend("the secret store is corrupt", err))?;
                if document.version != 1 {
                    return Err(SecretError::Backend(format!(
                        "the secret store has unsupported version {}",
                        document.version
                    )));
                }
                Ok(Loaded {
                    identity,
                    entries: document.entries,
                })
            }
        }
    }

    fn read_identity(&self) -> Result<Option<age::x25519::Identity>, SecretError> {
        // A key file widened outside the helpers tightens again on the
        // next read; a filesystem that refuses is not an error here.
        tighten_file(&self.key);
        match std::fs::read_to_string(&self.key) {
            Ok(text) => text
                .trim()
                .parse()
                .map(Some)
                .map_err(|err| backend("the secrets key is not a valid age identity", err)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(backend("cannot read the secrets key", err)),
        }
    }

    /// Encrypts the map to the identity's public key and writes it,
    /// creating the identity first when this store has none.
    fn persist(&self, loaded: &Loaded) -> Result<(), SecretError> {
        if let Some(parent) = self.key.parent() {
            private_dir(parent).map_err(|err| backend("cannot create the state directory", err))?;
        }
        if let Some(parent) = self.ciphertext.parent() {
            private_dir(parent)
                .map_err(|err| backend("cannot create the config directory", err))?;
        }
        if !self.key.exists() {
            private_write_atomic(
                &self.key,
                loaded.identity.to_string().expose_secret().as_bytes(),
            )
            .map_err(|err| backend("cannot write the secrets key", err))?;
        }
        let document = Document {
            version: 1,
            entries: loaded.entries.clone(),
        };
        let plaintext = serde_json::to_vec(&document)
            .map_err(|err| backend("cannot serialize the secret store", err))?;
        let recipient = loaded.identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .map_err(|err| backend("cannot encrypt the secret store", err))?;
        let mut ciphertext = Vec::new();
        let mut writer = encryptor
            .wrap_output(&mut ciphertext)
            .map_err(|err| backend("cannot encrypt the secret store", err))?;
        writer
            .write_all(&plaintext)
            .map_err(|err| backend("cannot encrypt the secret store", err))?;
        writer
            .finish()
            .map_err(|err| backend("cannot encrypt the secret store", err))?;
        private_write_atomic(&self.ciphertext, ciphertext)
            .map_err(|err| backend("cannot write the secret store", err))
    }

    /// The cross-process lock around a write. The lock file exists only
    /// for the duration of the write; a stale one older than thirty
    /// seconds is removed so a crash cannot wedge the store forever.
    ///
    /// The wait is capped at a quarter second: this seam is synchronous,
    /// so a polling loop cannot yield, and the frontends call `set` from
    /// async turn setup. A writer that holds the lock longer than that
    /// (or a runtime whose threads cannot sleep at all) gets the error
    /// naming the lock path instead of stalling the caller for seconds.
    fn lock(&self) -> Result<LockGuard, SecretError> {
        let path = self.lock_path();
        if let Some(parent) = path.parent() {
            private_dir(parent)
                .map_err(|err| backend("cannot create the config directory", err))?;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match private_create(&path) {
                Ok(_) => return Ok(LockGuard { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(SecretError::Backend(format!(
                            "another rhema process is writing the secret store, or a stale \
                             lock remains at {}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(backend("cannot lock the secret store", err)),
            }
        }
    }

    fn lock_path(&self) -> PathBuf {
        let name = self
            .ciphertext
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("secrets.age");
        self.ciphertext.with_file_name(format!(".{name}.lock"))
    }
}

impl SecretStore for EncryptedFileSecretStore {
    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        Ok(self.load()?.entries.get(id).cloned())
    }

    fn set(&self, id: &str, value: &str) -> Result<(), SecretError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| SecretError::Backend("the secret store lock is poisoned".into()))?;
        let _lock = self.lock()?;
        let mut loaded = self.load()?;
        loaded.entries.insert(id.to_owned(), value.to_owned());
        self.persist(&loaded)
    }

    fn delete(&self, id: &str) -> Result<bool, SecretError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| SecretError::Backend("the secret store lock is poisoned".into()))?;
        let _lock = self.lock()?;
        let mut loaded = self.load()?;
        if loaded.entries.remove(id).is_none() {
            return Ok(false);
        }
        self.persist(&loaded)?;
        Ok(true)
    }

    fn reachable(&self) -> bool {
        // A ciphertext without its key answers no before any probe.
        if self.load().is_err() {
            return false;
        }
        // The read side can be healthy while the write side is not (a
        // read-only config or state root), and that is exactly the case
        // where a login would fall back to plaintext. Probe both roots
        // with a private throwaway file, then remove it.
        let probe = format!(".secrets-reachability-{}.tmp", fresh_id());
        for parent in [self.ciphertext.parent(), self.key.parent()] {
            let Some(parent) = parent else {
                return false;
            };
            if private_dir(parent).is_err() {
                return false;
            }
            let path = parent.join(&probe);
            let created = private_create(&path).is_ok();
            let _ = std::fs::remove_file(&path);
            if !created {
                return false;
            }
        }
        true
    }
}

/// The write lock, removed on drop.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a lock file is old enough to be a crash left it behind.
fn stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(30))
}

fn backend(context: &str, err: impl std::fmt::Display) -> SecretError {
    SecretError::Backend(format!("{context}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::contract;

    fn store(dir: &tempfile::TempDir) -> EncryptedFileSecretStore {
        EncryptedFileSecretStore::new(
            dir.path().join("config/secrets.age"),
            dir.path().join("state/secrets.key"),
        )
    }

    #[test]
    fn the_file_store_replays_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        contract::run_all(&store(&dir));
    }

    #[test]
    fn values_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = store(&dir);
        first.set("ref-1", "sk-live-secret").unwrap();
        drop(first);
        let second = store(&dir);
        assert_eq!(
            second.get("ref-1").unwrap().as_deref(),
            Some("sk-live-secret")
        );
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.set("ref-1", "sk-live-secret").unwrap();
        let bytes = std::fs::read(&store.ciphertext).unwrap();
        assert!(
            !bytes
                .windows(b"sk-live-secret".len())
                .any(|window| window == b"sk-live-secret"),
            "the plaintext must not land in the age file"
        );
    }

    #[test]
    fn a_missing_key_does_not_overwrite_the_ciphertext() {
        let dir = tempfile::tempdir().unwrap();
        let first = store(&dir);
        first.set("ref-1", "sk-live-secret").unwrap();
        let before = std::fs::read(&first.ciphertext).unwrap();
        std::fs::remove_file(&first.key).unwrap();
        let reopened = store(&dir);
        assert!(!reopened.reachable());
        assert!(matches!(
            reopened.get("ref-1"),
            Err(SecretError::MissingKey { .. })
        ));
        assert!(reopened.set("ref-2", "another").is_err());
        assert_eq!(std::fs::read(&first.ciphertext).unwrap(), before);
    }

    #[test]
    fn a_wrong_key_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.set("ref-1", "sk-live-secret").unwrap();
        let other = age::x25519::Identity::generate();
        private_write_atomic(&store.key, other.to_string().expose_secret().as_bytes()).unwrap();
        assert!(!store.reachable());
        assert!(store.get("ref-1").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store.set("ref-1", "sk-live-secret").unwrap();
        for path in [&store.ciphertext, &store.key] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        for dir in [
            store.key.parent().unwrap(),
            store.ciphertext.parent().unwrap(),
        ] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    #[test]
    fn concurrent_writers_keep_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut handles = Vec::new();
        for writer in 0..2 {
            let root = dir.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                let store = EncryptedFileSecretStore::new(
                    root.join("config/secrets.age"),
                    root.join("state/secrets.key"),
                );
                for n in 0..8 {
                    store.set(&format!("w{writer}-{n}"), "v").unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let store = store(&dir);
        for writer in 0..2 {
            for n in 0..8 {
                assert_eq!(
                    store.get(&format!("w{writer}-{n}")).unwrap().as_deref(),
                    Some("v")
                );
            }
        }
    }

    #[test]
    fn reachability_does_not_create_the_store_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        assert!(store.reachable());
        assert!(!store.ciphertext.exists());
        assert!(!store.key.exists());
    }
}
