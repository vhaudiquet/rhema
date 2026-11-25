//! One init file per session's kit set, and the directory that owns it.
//!
//! The five surfaces (jobs, vision, search, decision, MCP) used to be
//! five chained `BASH_ENV` files. They are one file now: the stub text
//! is static, the per-session values are env. A [`Registry`] keeps the
//! placed files alive for the app's lifetime and removes their
//! directories when the app drops.
//!
//! See `specs/rhema-runtime-kit.md`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use rhema_bash::runtime::{Kit, init_text};

/// The init file `BASH_ENV` names, and the env map that points at it.
pub(crate) struct Placed {
    /// Every kit's vars plus `BASH_ENV`.
    pub env: BTreeMap<String, String>,
    dir: PathBuf,
}

impl Drop for Placed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The placed files by content key. One app owns one registry, so its
/// drop is the cleanup: every directory it created goes with it.
#[derive(Default)]
pub(crate) struct Registry {
    placed: Mutex<BTreeMap<u64, Arc<Placed>>>,
}

impl Registry {
    /// An empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Places `kits`' one init file, or returns the file already placed
    /// for the same stubs, vars, and inherited `BASH_ENV`. `None` when
    /// the temp directory or the file cannot be written: the caller
    /// then advertises no kit, the fail-closed answer.
    pub(crate) fn place(&self, kits: &[Kit], inherited: Option<&OsStr>) -> Option<Arc<Placed>> {
        let key = key(kits, inherited);
        {
            let placed = self.placed.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(found) = placed.get(&key) {
                return Some(found.clone());
            }
        }
        let dir = rhema_bash::init::temp_dir("rhema-kit").ok()?;
        let init = dir.join("init.bash");
        let mut text = String::new();
        if let Some(inherited) = inherited {
            let quoted = inherited.to_string_lossy().replace('\'', "'\\''");
            text.push_str(&format!("source '{quoted}' || true\n"));
        }
        text.push_str(&init_text(kits));
        if std::fs::write(&init, text).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o600));
        }
        let mut env = BTreeMap::new();
        for kit in kits {
            env.extend(kit.vars.clone());
        }
        env.insert(
            rhema_bash::init::VAR_BASH_ENV.to_owned(),
            init.to_string_lossy().into_owned(),
        );
        let placed = Arc::new(Placed { env, dir });
        self.placed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, placed.clone());
        Some(placed)
    }
}

/// The content key: the stub file and the env values, which are the
/// only things that make one placement differ from another.
fn key(kits: &[Kit], inherited: Option<&OsStr>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for kit in kits {
        kit.feature.name().hash(&mut hasher);
        for stub in &kit.stubs {
            stub.name.hash(&mut hasher);
            stub.bin_var.hash(&mut hasher);
            for arg in &stub.args {
                arg.hash(&mut hasher);
            }
        }
        for (name, value) in &kit.vars {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
    }
    inherited.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_bash::runtime::{Feature, Needs, Stub};

    fn kit() -> Kit {
        Kit::new(
            Feature::Jobs,
            vec![Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap()],
            Needs::MACRO,
        )
        .with_vars(BTreeMap::from([("RHEMA_SESSION".into(), "s1".into())]))
    }

    #[test]
    fn one_file_carries_the_stub_and_the_env_points_at_it() {
        let registry = Registry::new();
        let placed = registry.place(&[kit()], None).unwrap();
        let text = std::fs::read_to_string(&placed.env["BASH_ENV"]).unwrap();
        assert!(text.contains("job()"), "{text}");
        assert_eq!(placed.env["RHEMA_SESSION"], "s1");
    }

    #[test]
    fn the_inherited_bash_env_is_sourced_first() {
        let registry = Registry::new();
        let placed = registry
            .place(&[kit()], Some(OsStr::new("/tmp/user.bash")))
            .unwrap();
        let text = std::fs::read_to_string(&placed.env["BASH_ENV"]).unwrap();
        assert!(
            text.starts_with("source '/tmp/user.bash' || true\n"),
            "{text}"
        );
    }

    #[test]
    fn the_same_kit_set_reuses_the_placed_file() {
        let registry = Registry::new();
        let first = registry.place(&[kit()], None).unwrap();
        let second = registry.place(&[kit()], None).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn the_registry_drop_removes_the_directory() {
        let dir = {
            let registry = Registry::new();
            registry.place(&[kit()], None).unwrap().dir.clone()
        };
        assert!(!dir.exists(), "the placement directory is gone");
    }
}
