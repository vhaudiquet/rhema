//! Where a runtime kit can land: the capability gate and the path
//! layout, kept pure so every backend answers the same question the
//! same way.
//!
//! See `specs/rhema-runtime-kit.md`.

use std::path::{Path, PathBuf};

use rhema_bash::runtime::{Kit, Needs};

/// What a backend offers the kit machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// A directory both sides see, when the backend can mount one.
    pub shared_root: bool,
    /// Commands can run in the sandbox.
    pub exec: bool,
    /// Files can be read back to the host.
    pub pull: bool,
}

impl Capabilities {
    /// A mounted backend: a shared root, exec, and pull.
    pub const MOUNTED: Self = Self {
        shared_root: true,
        exec: true,
        pull: true,
    };
    /// A sandbox with no shared path: exec and pull only.
    pub const SANDBOX: Self = Self {
        shared_root: false,
        exec: true,
        pull: true,
    };
    /// A sandbox whose files cannot be read back either.
    pub const SEALED: Self = Self {
        shared_root: false,
        exec: true,
        pull: false,
    };
    /// Nothing runs.
    pub const NONE: Self = Self {
        shared_root: false,
        exec: false,
        pull: false,
    };
}

/// The shared root a mounted backend exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoot {
    /// The host-side path.
    pub path: PathBuf,
    /// Where the sandbox sees it, for the env and the scripts.
    pub sandbox_path: String,
}

impl SharedRoot {
    /// A root at `path`, seen at the same path inside the sandbox.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let sandbox_path = path.to_string_lossy().into_owned();
        Self { path, sandbox_path }
    }

    /// The helper install for one kit version.
    pub fn install_dir(&self, version: &str) -> PathBuf {
        self.path.join("bin").join(version)
    }

    /// The init file for one kit, beside the helper.
    pub fn init_path(&self, version: &str, kit: &str) -> PathBuf {
        self.install_dir(version).join(format!("init-{kit}.sh"))
    }

    /// The per-session tree: vision staging, jobs state, MCP media.
    pub fn session_dir(&self, session: &str) -> PathBuf {
        self.path.join("sessions").join(session)
    }

    /// The MCP socket's directory for one session.
    pub fn mcp_dir(&self, session: &str) -> PathBuf {
        self.path.join("mcp").join(session)
    }
}

/// Where a kit's helper and init live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// The shared root is mounted in the sandbox.
    Install,
    /// No shared root: a sandbox-private cache.
    Fallback,
}

/// Where one kit lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Under the shared root, read directly by the host.
    Shared,
    /// In the sandbox's own filesystem, pulled by the host.
    Sandbox,
    /// The backend cannot carry this kit; the reason goes to the
    /// debug notice.
    Skip(&'static str),
}

/// The gate: needs against capabilities, one answer per kit.
///
/// A socket is the exception: it needs one live path both sides
/// reach, so a sandbox with no shared root skips MCP and keeps the
/// other four.
pub fn decide(needs: &Needs, caps: &Capabilities) -> Decision {
    if needs.socket {
        return if caps.shared_root {
            Decision::Shared
        } else {
            Decision::Skip("the mcp socket needs a shared path")
        };
    }
    if needs.helper && !caps.exec {
        return Decision::Skip("the backend cannot execute the helper");
    }
    // A mounted backend installs once into the shared root and every
    // helper kit uses that copy; the sandbox-private cache is the
    // fallback, for search and decide where the host needs no state.
    if needs.helper && caps.shared_root {
        return Decision::Shared;
    }
    if needs.state && !caps.exec {
        return Decision::Skip("no sandbox process can create the state");
    }
    if needs.host_read {
        return if caps.shared_root {
            Decision::Shared
        } else if caps.pull {
            Decision::Sandbox
        } else {
            Decision::Skip("the host cannot read the sandbox state")
        };
    }
    Decision::Sandbox
}

/// The gate for every kit at once, in input order.
pub fn decide_all<'a>(
    kits: impl IntoIterator<Item = &'a Kit>,
    caps: &Capabilities,
) -> Vec<(rhema_bash::runtime::Feature, Decision)> {
    kits.into_iter()
        .map(|kit| (kit.feature, decide(&kit.needs, caps)))
        .collect()
}

/// Writes `kits`' init file at `path`, or under `dir/init.sh`. The
/// caller owns the directory's cleanup.
///
/// # Errors
///
/// The directory or the file cannot be written.
pub fn write_init(dir: &Path, kits: &[Kit]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("init.sh");
    std::fs::write(&path, rhema_bash::runtime::init_text(kits))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_bash::runtime::Feature;

    fn kit(feature: Feature, needs: Needs) -> Kit {
        Kit::new(feature, Vec::new(), needs)
    }

    #[test]
    fn a_mounted_backend_carries_every_kit() {
        let decisions = decide_all(
            &[
                kit(Feature::Jobs, Needs::MACRO),
                kit(Feature::Vision, Needs::MACRO),
                kit(Feature::Search, Needs::HOST_CALL),
                kit(Feature::Mcp, Needs::MCP),
            ],
            &Capabilities::MOUNTED,
        );
        assert!(
            decisions
                .iter()
                .all(|(_, decision)| *decision == Decision::Shared),
            "{decisions:?}"
        );
    }

    #[test]
    fn a_pulling_sandbox_keeps_jobs_and_vision_but_loses_mcp() {
        let jobs = decide(&Needs::MACRO, &Capabilities::SANDBOX);
        assert_eq!(jobs, Decision::Sandbox);
        let mcp = decide(&Needs::MCP, &Capabilities::SANDBOX);
        assert_eq!(mcp, Decision::Skip("the mcp socket needs a shared path"));
    }

    #[test]
    fn a_sealed_sandbox_keeps_only_the_helper_kits() {
        assert_eq!(
            decide(&Needs::MACRO, &Capabilities::SEALED),
            Decision::Skip("the host cannot read the sandbox state")
        );
        assert_eq!(
            decide(&Needs::HOST_CALL, &Capabilities::SEALED),
            Decision::Sandbox
        );
    }

    #[test]
    fn no_exec_skips_everything_that_needs_the_helper() {
        assert_eq!(
            decide(&Needs::MACRO, &Capabilities::NONE),
            Decision::Skip("the backend cannot execute the helper")
        );
    }

    #[test]
    fn the_init_file_lands_at_the_kit_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_init(dir.path(), &[kit(Feature::Jobs, Needs::MACRO)]).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("runtime kit v"), "{text}");
    }

    #[test]
    fn the_shared_root_paths_stay_under_the_root() {
        let root = SharedRoot::new("/run/user/1000/rhema");
        assert_eq!(
            root.init_path("1", "jobs"),
            PathBuf::from("/run/user/1000/rhema/bin/1/init-jobs.sh")
        );
        assert_eq!(
            root.session_dir("s1"),
            PathBuf::from("/run/user/1000/rhema/sessions/s1")
        );
        assert_eq!(
            root.mcp_dir("s1"),
            PathBuf::from("/run/user/1000/rhema/mcp/s1")
        );
    }
}
