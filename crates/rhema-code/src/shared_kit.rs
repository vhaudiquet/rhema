//! The jobs and vision kits for a mounted sandbox: their state lives
//! under the shared session tree, not a host temp dir, and the wirings
//! must outlive the setup call that builds the kit list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rhema_sandbox::kit::SharedRoot;

/// The two wirings, each held for the process's life by [`wire`]'s
/// registry: dropping one deletes its state directory, so the map is
/// the owner the agent's shells cannot see.
#[derive(Clone, Default)]
pub(crate) struct Wirings {
    pub jobs: Option<Arc<rhema_bash::jobs::Wiring>>,
    pub vision: Option<Arc<rhema_bash::vision::Wiring>>,
}

static WIRED: OnceLock<Mutex<BTreeMap<String, Wirings>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<String, Wirings>> {
    WIRED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The session's wirings, built once per key. `key` is the runtime
/// session key [`crate::mcp_wire::runtime_session_key`] returns, the
/// same name [`rhema_sandbox::kit::place_shared`] places the init under.
pub(crate) fn wire(root: &SharedRoot, key: &str, jobs_tag: &str) -> Wirings {
    let mut registry = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(wired) = registry.get(key) {
        return wired.clone();
    }
    let jobs = root.session_dir(key).join("jobs");
    let jobs = match rhema_bash::jobs::provision_shared(jobs, jobs_tag) {
        Ok(wiring) => Some(Arc::new(wiring)),
        Err(err) => {
            eprintln!("warning: the sandbox jobs state did not provision: {err}");
            None
        }
    };
    let vision = root.session_dir(key).join("vision");
    let vision = match rhema_bash::vision::provision_shared(vision) {
        Ok(wiring) => Some(Arc::new(wiring)),
        Err(err) => {
            eprintln!("warning: the sandbox vision state did not provision: {err}");
            None
        }
    };
    let wired = Wirings { jobs, vision };
    registry.insert(key.to_owned(), wired.clone());
    wired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_session_provisions_jobs_and_vision_under_its_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = SharedRoot::new(dir.path());
        let wired = wire(&root, "k1", "tag");
        assert!(wired.jobs.is_some(), "jobs wires without a host gob");
        assert!(wired.vision.is_some());
        assert!(root.session_dir("k1").join("jobs/session").is_file());
        assert!(root.session_dir("k1").join("vision/staging").is_dir());
        let again = wire(&root, "k1", "tag");
        assert_eq!(
            again.jobs.as_ref().map(Arc::as_ptr),
            wired.jobs.as_ref().map(Arc::as_ptr)
        );
        assert_eq!(
            again.vision.as_ref().map(Arc::as_ptr),
            wired.vision.as_ref().map(Arc::as_ptr)
        );
    }
}
