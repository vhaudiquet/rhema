//! Where a runtime kit can land: the capability gate and the path
//! layout, kept pure so every backend answers the same question the
//! same way.
//!
//! See `specs/rhema-runtime-kit.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rhema_bash::runtime::{Feature, KIT_VERSION, Kit, Needs};

/// What a backend offers the kit machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// A directory both sides see, when the backend can mount one.
    pub shared_root: Option<SharedRoot>,
    /// `Sandbox::write_file` can carry a helper and an init file.
    pub upload: bool,
    /// The host can read the sandbox's files (`read_file`).
    pub pull: bool,
    /// The sandbox can dial a host TCP endpoint.
    pub inbound_net: bool,
}

impl Capabilities {
    /// A backend with a mounted shared root.
    pub fn mounted(root: SharedRoot) -> Self {
        Self {
            shared_root: Some(root),
            upload: true,
            pull: true,
            inbound_net: false,
        }
    }

    /// A sandbox with no shared path: upload and pull.
    pub const SANDBOX: Self = Self {
        shared_root: None,
        upload: true,
        pull: true,
        inbound_net: false,
    };

    /// A sandbox whose files cannot be read back either.
    pub const SEALED: Self = Self {
        shared_root: None,
        upload: false,
        pull: false,
        inbound_net: false,
    };

    /// A remote box that can dial a host endpoint.
    pub const NET: Self = Self {
        shared_root: None,
        upload: true,
        pull: true,
        inbound_net: true,
    };
}

/// The shared root a mounted backend exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoot {
    /// The host-side path; both sides read and write it.
    pub host: PathBuf,
    /// The same directory as the sandbox sees it.
    pub sandbox: String,
}

impl SharedRoot {
    /// A root at `path`, seen at the same path inside the sandbox.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let host = path.into();
        let sandbox = host.to_string_lossy().into_owned();
        Self { host, sandbox }
    }

    /// The helper install for one kit version.
    pub fn install_dir(&self, version: &str) -> PathBuf {
        self.host.join("bin").join(version)
    }

    /// The init file for one kit, beside the helper.
    pub fn init_path(&self, version: &str, kit: &str) -> PathBuf {
        self.install_dir(version).join(format!("init-{kit}.sh"))
    }

    /// The per-session tree: vision staging, jobs state, MCP media.
    pub fn session_dir(&self, session: &str) -> PathBuf {
        self.host.join("sessions").join(session)
    }

    /// The MCP socket's directory for one session.
    pub fn mcp_dir(&self, session: &str) -> PathBuf {
        self.host.join("mcp").join(session)
    }
}

/// Where a kit's helper and init live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// A directory mounted into the sandbox (docker, lxd, ecrou).
    Shared(SharedRoot),
    /// No shared directory: a sandbox-side cache (ssh, http).
    Sandbox,
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
    if needs.bridge {
        return if caps.shared_root.is_some() {
            // v1 is the UDS on the mount; the TCP fallback is the
            // next transport, not a skipped kit.
            Decision::Shared
        } else {
            Decision::Skip("the mcp socket needs a shared path")
        };
    }
    if needs.exec && !caps.upload && caps.shared_root.is_none() {
        return Decision::Skip("the backend cannot place the helper");
    }
    // One install under the shared root serves every helper kit; the
    // sandbox cache is for the backends without one.
    if needs.exec && caps.shared_root.is_some() {
        return Decision::Shared;
    }
    if needs.shared_state {
        return if caps.shared_root.is_some() {
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

/// Why a kit placement did not land.
#[derive(Debug, thiserror::Error)]
pub enum KitError {
    /// The filesystem refused a step.
    #[error("kit placement: {0}")]
    Io(#[from] std::io::Error),
    /// The helper did not answer, or answered a failed run.
    #[error("the installed helper did not answer: {0}")]
    Probe(String),
    /// The install's version disagrees with this host.
    #[error("the installed helper is kit version {found}, this host needs {wanted}")]
    Version {
        /// The version the install names.
        found: String,
        /// The version this host generates.
        wanted: u32,
    },
}

/// A placed session: the env the shell needs, the init file, the
/// install path, the kits that were skipped, and the per-session dir,
/// which removes itself on drop.
#[derive(Debug)]
pub struct Placed {
    /// The env to merge into the shell's map.
    pub env: BTreeMap<String, String>,
    /// The init file `BASH_ENV` names.
    pub init: PathBuf,
    /// The installed helper, sandbox path.
    pub helper: String,
    /// Kits this backend could not carry, with the reason.
    pub skipped: Vec<(Feature, &'static str)>,
    /// The session tree; dropping it removes the tree best effort.
    pub session: SessionDir,
}

/// A per-session tree with a drop guard. Host-side only: a sandbox
/// path the host cannot reach is removed by the sandbox's own guard
/// (the placer's `rm -rf` on drop, or the start sweep).
#[derive(Debug)]
pub struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    /// The host path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Places `kits` under a shared root every side sees: installs the
/// helper once per version, writes the session tree and its init file,
/// and returns the env. Kits the gate refuses are listed in `skipped`
/// and left out of both the init and the env.
///
/// The helper is a host build copied into the shared root. An
/// install from a release archive for the sandbox's own architecture
/// is the next step; a wrong-architecture copy is caught when the
/// install's `__kit-version` runs in the sandbox.
///
/// # Errors
///
/// A directory or file cannot be written, or an existing install
/// names another kit version.
pub fn ensure_shared(
    shared: &SharedRoot,
    session: &str,
    helper: &Path,
    kits: &[Kit],
    caps: &Capabilities,
) -> Result<Placed, KitError> {
    let mut decided = Vec::new();
    let mut skipped = Vec::new();
    for kit in kits {
        match decide(&kit.needs, caps) {
            Decision::Shared => decided.push(kit.clone()),
            Decision::Sandbox => {
                // A shared placement cannot carry a kit whose state
                // must stay sandbox-side; the caller uses the
                // sandbox placement for those.
                skipped.push((kit.feature, "needs a sandbox-side placement"));
            }
            Decision::Skip(reason) => skipped.push((kit.feature, reason)),
        }
    }

    let version = KIT_VERSION.to_string();
    let install = shared.install_dir(&version);
    std::fs::create_dir_all(&install)?;
    let version_file = install.join(".kit-version");
    if version_file.is_file() {
        let found = std::fs::read_to_string(&version_file)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !found.is_empty() && found != version {
            return Err(KitError::Version {
                found,
                wanted: KIT_VERSION,
            });
        }
    }
    let target = install.join("rhema");
    if !target.is_file() {
        let staged = install.join("rhema.tmp");
        std::fs::copy(helper, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staged, &target)?;
    }
    std::fs::write(&version_file, &version)?;

    let session_dir = shared.session_dir(session);
    for sub in ["vision/staging", "jobs", "mcp"] {
        std::fs::create_dir_all(session_dir.join(sub))?;
    }
    let session_file = session_dir.join("jobs/session");
    std::fs::write(&session_file, session)?;

    let init = session_dir.join("init.sh");
    std::fs::write(&init, rhema_bash::runtime::init_text(&decided))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o600))?;
    }

    let sandbox_root = Path::new(&shared.sandbox);
    let sandbox_install = sandbox_root.join("bin").join(&version);
    let sandbox_session = sandbox_root.join("sessions").join(session);
    let helper = sandbox_install.join("rhema").to_string_lossy().into_owned();
    let mut env = BTreeMap::new();
    env.insert(
        "BASH_ENV".to_owned(),
        sandbox_session
            .join("init.sh")
            .to_string_lossy()
            .into_owned(),
    );
    for kit in &decided {
        for stub in &kit.stubs {
            env.insert(stub.bin_var.clone(), helper.clone());
        }
        for (key, value) in &kit.vars {
            env.insert(key.clone(), value.clone());
        }
    }
    env.insert(
        "RHEMA_VISION_STAGING".to_owned(),
        sandbox_session
            .join("vision/staging")
            .to_string_lossy()
            .into_owned(),
    );
    env.insert(
        "RHEMA_JOBS_SESSION".to_owned(),
        sandbox_session
            .join("jobs/session")
            .to_string_lossy()
            .into_owned(),
    );
    env.insert("RHEMA_SESSION".to_owned(), session.to_owned());

    Ok(Placed {
        env,
        init,
        helper,
        skipped,
        session: SessionDir { path: session_dir },
    })
}

/// Runs the installed helper in the sandbox and checks that it answers
/// this host's [`KIT_VERSION`]. This is the check that catches a copy
/// for the wrong architecture, and a install that is present but
/// broken.
///
/// # Errors
///
/// The sandbox cannot run the helper, the helper exits nonzero, or
/// its version differs.
pub async fn verify(sandbox: &dyn crate::Sandbox, shared: &SharedRoot) -> Result<(), KitError> {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let version = KIT_VERSION.to_string();
    let helper = Path::new(&shared.sandbox)
        .join("bin")
        .join(&version)
        .join("rhema");
    let request = crate::RunRequest {
        command: format!("{} __kit-version", shquote(&helper.to_string_lossy())),
        cwd: "/".into(),
        timeout: Duration::from_secs(10),
        env: BTreeMap::new(),
    };
    let mut progress = rhema_core::OutputOnly(|_| {});
    let outcome = sandbox
        .run(request, &AtomicBool::new(false), &mut progress)
        .await
        .map_err(|err| KitError::Probe(err.to_string()))?;
    if outcome.code != Some(0) {
        return Err(KitError::Probe(format!(
            "the helper exited {:?}",
            outcome.code
        )));
    }
    let found = String::from_utf8_lossy(&outcome.stdout).trim().to_owned();
    if found != version {
        return Err(KitError::Version {
            found,
            wanted: KIT_VERSION,
        });
    }
    Ok(())
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

/// Single-quotes a word for the sandbox's shell.
fn shquote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_bash::runtime::{Feature, Stub};

    fn kit(feature: Feature, needs: Needs) -> Kit {
        Kit::new(feature, Vec::new(), needs)
    }

    #[test]
    fn a_mounted_backend_carries_every_kit() {
        let dir = tempfile::tempdir().unwrap();
        let caps = Capabilities::mounted(SharedRoot::new(dir.path()));
        let decisions = decide_all(
            &[
                kit(Feature::Jobs, Needs::MACRO),
                kit(Feature::Vision, Needs::MACRO),
                kit(Feature::Search, Needs::HOST_CALL),
                kit(Feature::Mcp, Needs::MCP),
            ],
            &caps,
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
    fn a_sealed_sandbox_keeps_nothing() {
        assert_eq!(
            decide(&Needs::MACRO, &Capabilities::SEALED),
            Decision::Skip("the backend cannot place the helper")
        );
        assert_eq!(
            decide(&Needs::HOST_CALL, &Capabilities::SEALED),
            Decision::Skip("the backend cannot place the helper")
        );
    }

    #[test]
    fn a_shared_placement_writes_the_env_the_init_and_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("host-rhema");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        let shared = SharedRoot::new(dir.path().join("runtime"));
        let kit = Kit::new(
            Feature::Jobs,
            vec![Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap()],
            Needs::MACRO,
        );
        let placed = ensure_shared(
            &shared,
            "s1",
            &helper,
            &[kit],
            &Capabilities::mounted(SharedRoot::new(dir.path())),
        )
        .unwrap();
        assert!(
            placed.env["BASH_ENV"].ends_with("/sessions/s1/init.sh"),
            "{:?}",
            placed.env
        );
        assert!(
            placed.env["RHEMA_JOBS_BIN"].ends_with("/bin/1/rhema"),
            "{:?}",
            placed.env
        );
        assert_eq!(placed.env["RHEMA_SESSION"], "s1");
        let text = std::fs::read_to_string(&placed.init).unwrap();
        assert!(text.contains("job()"), "{text}");
        assert!(shared.install_dir("1").join("rhema").is_file());
        let session = placed.session.path().to_path_buf();
        assert!(session.join("jobs/session").is_file());
        drop(placed);
        assert!(!session.exists(), "the session tree is gone");
    }

    #[tokio::test]
    async fn the_probe_reads_the_installed_version() {
        use crate::mock::ScriptedSandbox;
        let sandbox = ScriptedSandbox::new();
        sandbox.push_run(Ok(crate::RunOutcome {
            stdout: b"1\n".to_vec(),
            stderr: Vec::new(),
            code: Some(0),
        }));
        let dir = tempfile::tempdir().unwrap();
        verify(&sandbox, &SharedRoot::new(dir.path()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_wrong_version_is_refused() {
        use crate::mock::ScriptedSandbox;
        let sandbox = ScriptedSandbox::new();
        sandbox.push_run(Ok(crate::RunOutcome {
            stdout: b"2\n".to_vec(),
            stderr: Vec::new(),
            code: Some(0),
        }));
        let dir = tempfile::tempdir().unwrap();
        let err = verify(&sandbox, &SharedRoot::new(dir.path()))
            .await
            .unwrap_err();
        assert!(matches!(err, KitError::Version { .. }), "{err}");
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
