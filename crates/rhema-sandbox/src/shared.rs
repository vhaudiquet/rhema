//! Cross-process sharing of one named sandbox instance: the file lock
//! every holder of a `rhema-sandbox-*` instance takes, and the
//! stop-on-exit answer it gives.
//!
//! Two rhema processes in one workspace derive the same instance name,
//! so "stop the instance when the session ends" has to know how many
//! sessions are live — and a counter kept anywhere forgets what a
//! crash knew. The lock file replaces the counter: each process holds
//! an advisory lock on one file per instance for the length of its
//! session, and the kernel drops the hold when the process dies, so a
//! crash cannot leak a count. The exit asks the lock a question
//! instead of doing arithmetic: upgrading from shared to exclusive
//! succeeds only for the last holder, and that is the one process
//! allowed to stop the instance.
//!
//! The discipline every holder follows:
//!
//! 1. [`InstanceLock::acquire`] — blocking exclusive — around the
//!    whole assembly (launch, devices, idmap repair, provision, seed):
//!    the mutation window, serialized across processes. The wait is
//!    bounded by the work itself, every step of which already runs
//!    under a timeout — a process assembling behind a cold first run
//!    waits out the provision it would otherwise have raced.
//! 2. [`InstanceLock::share`] — downgrade to shared — for the
//!    session's life: use without exclusion.
//! 3. On the way out, [`InstanceLock::is_sole`] — a non-blocking
//!    upgrade — and stop only on `true`. A peer's shared hold fails
//!    the upgrade, and the failed upgrade leaves this holder shared.
//!
//! No holder is ever in session without having passed (1), so a stop
//! decided under exclusive cannot land under a live session: the
//! next-comer either joined before the upgrade (and blocks it) or is
//! serialized behind the stop and re-ensures a stopped instance, which
//! the assemblies already handle.
//!
//! The locks live under `XDG_RUNTIME_DIR` — per-user, tmpfs, wiped at
//! boot: the right lifetime for "who is using this right now" — with a
//! `/tmp` fallback for a broken setup. Lock files are never unlinked
//! (unlinking a locked file forks the mutex: latecomers would open a
//! fresh inode and hold nothing); they are zero bytes and there is one
//! per instance ever used, so the accumulation stays invisible.
//!
//! One hold serves the whole session: a re-assembly of the same
//! instance while its session still lives reuses the registry's file
//! instead of taking a second exclusive hold on the file this process
//! already owns — `flock` counts open file descriptions, not
//! processes, so that second hold would block against the session's
//! own shared lock and hang. The registry holds the file weakly, so
//! the hold's lifetime is exactly the session's: the last session
//! lock dropped closes the descriptor, and a later assembly takes a
//! fresh exclusive window instead of leaning on a zombie hold that
//! would keep a peer's stop from ever firing.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// One instance's lock: an advisory exclusive-or-shared hold on the
/// instance's lock file. The descriptor is process-global — every
/// [`InstanceLock`] of one path in one process shares it — so the
/// states compose instead of contending with themselves.
#[derive(Debug)]
pub struct InstanceLock {
    file: Arc<File>,
}

impl InstanceLock {
    /// Takes the instance's mutation window: blocking exclusive, held
    /// across the whole assembly. Blocks up to that work's own length —
    /// the launch, the provision, the seed — and is the reason two
    /// rhema processes can never launch, device-add, or provision the
    /// same instance together.
    pub fn acquire(backend: &str, name: &str) -> io::Result<Self> {
        let path = Self::path(backend, name);
        if let Some(held) = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&path)
            .and_then(Weak::upgrade)
        {
            // This process already holds the instance, and its lock
            // state — shared, once assembled — is the session posture
            // the re-assembly wants. Re-taking exclusive here would
            // block on the process's own descriptor.
            return Ok(Self { file: held });
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = Arc::new(
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)?,
        );
        // Outside the registry's lock: the blocking wait (a peer's cold
        // provision, minutes at worst) must not pin the registry and
        // stall an unrelated instance's assembly in this process.
        file.lock()?;
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path, Arc::downgrade(&file));
        Ok(Self { file })
    }

    /// The lock file's path:
    /// `<runtime>/rhema/sandbox/<backend>-<name>.lock`.
    pub fn path(backend: &str, name: &str) -> PathBuf {
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("rhema").join("sandbox"),
            // No runtime dir: the fallback keeps per-user by name, the
            // same hygiene XDG_RUNTIME_DIR gives for free.
            _ => PathBuf::from("/tmp").join(format!(
                "rhema-sandbox-{}",
                std::env::var("USER").unwrap_or_else(|_| "user".to_owned())
            )),
        };
        dir.join(format!("{}-{}.lock", sanitize(backend), sanitize(name)))
    }

    /// Downgrades to the session posture: shared. The downgrade always
    /// succeeds — it is what every concurrent holder already holds.
    pub fn share(&self) -> io::Result<()> {
        self.file.lock_shared()
    }

    /// The stop-on-exit question: is this process the lock's last
    /// holder? On `true` the hold has been upgraded to exclusive —
    /// serialized against every comer behind it — and the caller
    /// should stop the instance; on `false` a peer lives on, the hold
    /// stays shared, and the instance is not this exit's to stop.
    pub fn is_sole(&self) -> bool {
        self.file.try_lock().is_ok()
    }
}

/// The per-process registry of held lock files, held weakly: the
/// path's descriptor while some session in this process still holds
/// it, a dead entry once none does. One entry per instance the
/// process ever used is the whole footprint — a lock file is zero
/// bytes, and a dead entry only costs its path.
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<File>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<File>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The filename piece of a backend or instance name: LXD and docker
/// names are already narrow, the filter keeps a hand-written config's
/// target from putting a `/` in the lock path.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Opens the lock file directly, the way a peer process would —
    /// the tests' stand-in holders never touch the registry.
    fn open_direct(backend: &str, name: &str) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(InstanceLock::path(backend, name))
            .expect("the lock file opens")
    }

    #[test]
    fn the_lock_path_lands_under_the_runtime_dir() {
        let path = InstanceLock::path("lxd", "rhema-sandbox-proj-0badc0de");
        // The file name carries both keys; the directory is the
        // runtime dir's `rhema/sandbox` or the /tmp fallback — which
        // one is the environment's answer, not the test's.
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("lxd-rhema-sandbox-proj-0badc0de.lock")
        );
        assert!(path.parent().is_some(), "the file sits in a directory");
        // A hand-written target with path separators cannot escape the
        // directory the lock lives in: separators become underscores
        // inside one flat filename, and the file lands beside every
        // other lock. The dots survive — inert text in one component.
        let hostile = InstanceLock::path("lxd", "../../etc/rhema");
        assert_eq!(hostile.parent(), path.parent());
        assert_eq!(
            hostile.file_name().and_then(|name| name.to_str()),
            Some("lxd-.._.._etc_rhema.lock")
        );
    }

    #[test]
    fn a_reacquire_while_the_session_lives_reuses_the_descriptor() {
        // The second acquire shares the first's descriptor: without the
        // registry it would block on this process's own lock and hang.
        let first = InstanceLock::acquire("lxd", "reuse-test").expect("the first hold");
        first.share().expect("the downgrade");
        let second = InstanceLock::acquire("lxd", "reuse-test").expect("the reuse");
        assert!(second.is_sole(), "no other process holds the file");
        // The registry holds the file weakly: with the session gone, a
        // fresh acquire is a fresh exclusive window, not the zombie of
        // a hold no session owns anymore.
        drop(second);
        drop(first);
        let fresh = InstanceLock::acquire("lxd", "reuse-test").expect("the fresh hold");
        assert!(fresh.is_sole());
    }

    #[test]
    fn a_peer_shared_hold_blocks_the_promotion_and_survives_it() {
        let lock = InstanceLock::acquire("docker", "peer-test").expect("the hold");
        lock.share().expect("the downgrade");
        let peer = open_direct("docker", "peer-test");
        peer.lock_shared().expect("the peer's shared hold");
        assert!(!lock.is_sole(), "the peer says not-sole");
        // The failed upgrade left this holder shared, not exclusive: a
        // fresh descriptor still takes the file shared, which exclusive
        // would refuse.
        let probe = open_direct("docker", "peer-test");
        probe
            .try_lock_shared()
            .expect("the failed upgrade left shared in place");
    }

    #[test]
    fn the_sole_holder_promotes_to_real_exclusivity() {
        let lock = InstanceLock::acquire("docker", "sole-test").expect("the hold");
        lock.share().expect("the downgrade");
        assert!(lock.is_sole(), "nobody else is here");
        // The promotion is exclusivity, not a no-op answer: a fresh
        // descriptor cannot even take the file shared now.
        let probe = open_direct("docker", "sole-test");
        assert!(
            probe.try_lock_shared().is_err(),
            "the promoted hold excludes a shared newcomer"
        );
    }

    #[test]
    fn acquire_waits_for_a_peer_exclusive_to_lift() {
        // The peer holds exclusive on its own descriptor, directly —
        // the registry must not shortcut a genuine cross-process wait.
        let peer = open_direct("lxd", "wait-test");
        peer.lock().expect("the peer's exclusive");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(peer);
        });
        let started = std::time::Instant::now();
        let lock = InstanceLock::acquire("lxd", "wait-test").expect("the wait ends");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(100),
            "acquire returned before the peer let go"
        );
        assert!(lock.is_sole(), "the acquired hold is exclusive");
        releaser.join().expect("the releaser joins");
    }
}
