//! Where a runtime kit can land: the capability gate and the path
//! layout, kept pure so every backend answers the same question the
//! same way.
//!
//! See `specs/rhema-runtime-kit.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    /// Whether a Unix socket on the share accepts `connect()` from the
    /// sandbox: true for a bind mount and a disk device, and for
    /// ecrou's virtiofs by default; a failed bind turns MCP off at
    /// runtime.
    pub unix_socket: bool,
}

impl SharedRoot {
    /// A root at `path`, seen at the same path inside the sandbox.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let host = path.into();
        let sandbox = host.to_string_lossy().into_owned();
        Self {
            host,
            sandbox,
            unix_socket: true,
        }
    }

    /// A root whose share carries files but not a working Unix socket.
    pub fn without_unix_socket(path: impl Into<PathBuf>) -> Self {
        Self {
            unix_socket: false,
            ..Self::new(path)
        }
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

/// The sandbox-side installer: a command that installs the helper
/// into the directory `{dir}` names. The host builds it from a
/// checksum-verified release or an uploaded tarball; the sandbox runs
/// it, so the helper matches the sandbox's OS and architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installer {
    /// The shell command; every `{dir}` is replaced by the quoted
    /// install directory, sandbox path.
    pub command: String,
}

impl Installer {
    /// An installer command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }

    fn for_dir(&self, dir: &str) -> String {
        self.command.replace("{dir}", &shquote(dir))
    }
}

/// How long an install or an init generation may run.
const INSTALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

/// Runs one helper command in the sandbox, with the install budget
/// and no env: the command names its paths itself.
async fn run_command(
    sandbox: &dyn crate::Sandbox,
    command: String,
    what: &str,
) -> Result<crate::RunOutcome, KitError> {
    use std::sync::atomic::AtomicBool;

    let request = crate::RunRequest {
        command,
        cwd: "/".into(),
        timeout: INSTALL_BUDGET,
        env: BTreeMap::new(),
    };
    let mut progress = rhema_core::OutputOnly(|_| {});
    let outcome = sandbox
        .run(request, &AtomicBool::new(false), &mut progress)
        .await
        .map_err(|err| KitError::Probe(format!("{what}: {err}")))?;
    if outcome.code != Some(0) {
        let detail = String::from_utf8_lossy(&outcome.stderr);
        let detail = detail.trim();
        return Err(KitError::Probe(if detail.is_empty() {
            format!("{what} exited {:?}", outcome.code)
        } else {
            format!("{what} exited {:?}: {detail}", outcome.code)
        }));
    }
    Ok(outcome)
}

/// Places `kits` under a shared root every side sees: runs the
/// sandbox-side installer once per kit version when the install is
/// missing, checks the installed helper's `__kit-version` with it,
/// and has the helper generate the session's init file. Kits the gate
/// refuses are listed in `skipped` and left out of both the init and
/// the env.
///
/// The host never copies its own binary here. The installer is the
/// sandbox's, built by the host from a release for the sandbox's
/// target; an install that cannot run is caught by the version probe.
///
/// # Errors
///
/// A directory or file cannot be written, the installer or the
/// helper fails, or the install names another kit version.
pub async fn ensure_shared(
    sandbox: &dyn crate::Sandbox,
    shared: &SharedRoot,
    session: &str,
    installer: &Installer,
    kits: &[Kit],
    caps: &Capabilities,
) -> Result<Placed, KitError> {
    let mut decided = Vec::new();
    let mut skipped = Vec::new();
    for kit in kits {
        match decide(&kit.needs, caps) {
            Decision::Shared => decided.push(kit.clone()),
            Decision::Sandbox => {
                skipped.push((kit.feature, "needs a sandbox-side placement"));
            }
            Decision::Skip(reason) => skipped.push((kit.feature, reason)),
        }
    }

    sweep(shared);
    let version = KIT_VERSION.to_string();
    let install = shared.install_dir(&version);
    let sandbox_install = Path::new(&shared.sandbox).join("bin").join(&version);
    let version_file = install.join(".kit-version");
    let installed = install.join("rhema").is_file()
        && std::fs::read_to_string(&version_file)
            .map(|found| found.trim() == version)
            .unwrap_or(false);
    if !installed {
        std::fs::create_dir_all(&install)?;
        run_command(
            sandbox,
            installer.for_dir(&sandbox_install.to_string_lossy()),
            "the kit installer",
        )
        .await?;
        std::fs::write(&version_file, &version)?;
    }
    verify(sandbox, shared).await?;

    let session_dir = shared.session_dir(session);
    for sub in ["vision/staging", "jobs", "mcp"] {
        std::fs::create_dir_all(session_dir.join(sub))?;
    }
    let session_file = session_dir.join("jobs/session");
    std::fs::write(&session_file, session)?;

    let sandbox_session = Path::new(&shared.sandbox).join("sessions").join(session);
    let helper = sandbox_install.join("rhema");
    let init = session_dir.join("init.sh");
    let sandbox_init = sandbox_session.join("init.sh");
    let mut command = format!("{} __init", shquote(&helper.to_string_lossy()));
    for kit in &decided {
        for stub in &kit.stubs {
            command.push_str(&format!(" --stub {}", shquote(&stub.flag())));
        }
    }
    command.push_str(&format!(" > {}", shquote(&sandbox_init.to_string_lossy())));
    run_command(sandbox, command, "the init generator").await?;

    let helper = helper.to_string_lossy().into_owned();
    let mut env = BTreeMap::new();
    env.insert(
        "BASH_ENV".to_owned(),
        sandbox_init.to_string_lossy().into_owned(),
    );
    for kit in &decided {
        for stub in &kit.stubs {
            env.insert(stub.bin_var.clone(), helper.clone());
        }
        for (key, value) in &kit.vars {
            env.insert(key.clone(), value.clone());
        }
    }
    // Last: a wiring that provisioned its own init file carries a
    // BASH_ENV pointing at a host temp path. The shared file is the
    // only one the sandbox can read.
    env.insert(
        rhema_bash::init::VAR_BASH_ENV.to_owned(),
        sandbox_session
            .join("init.sh")
            .to_string_lossy()
            .into_owned(),
    );
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

/// Places `kits` under a shared root the host can write directly:
/// creates the session tree, writes the one init file, and answers
/// the env map that points at it. No installer run and no guard: the
/// caller owns the session's lifetime, and a stale tree is 1 KB. This
/// is the sync half of [`ensure_shared`], for the frontends that place
/// after the install check.
///
/// # Errors
///
/// A directory or file cannot be written.
pub fn place_shared(shared: &SharedRoot, session: &str, kits: &[Kit]) -> std::io::Result<Placed> {
    sweep(shared);
    let host_session = shared.session_dir(session);
    for sub in ["vision/staging", "jobs", "mcp"] {
        std::fs::create_dir_all(host_session.join(sub))?;
    }
    let init = write_init(&host_session, kits)?;
    let helper = shared
        .install_dir(&KIT_VERSION.to_string())
        .join("rhema")
        .to_string_lossy()
        .into_owned();
    // `gob` lands beside the helper; the shell's PATH names that
    // directory so the jobs backend resolves.
    let helper_dir = Path::new(&helper)
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = std::fs::read_to_string(&init)?;
    std::fs::write(
        &init,
        format!("export PATH={}:\"$PATH\"\n{text}", shquote(&helper_dir)),
    )?;
    let sandbox_session = Path::new(&shared.sandbox).join("sessions").join(session);
    let mut env = BTreeMap::new();
    for kit in kits {
        for stub in &kit.stubs {
            env.insert(stub.bin_var.clone(), helper.clone());
        }
        for (key, value) in &kit.vars {
            env.insert(key.clone(), value.clone());
        }
    }
    // Last: a wiring that provisioned its own init file carries a
    // BASH_ENV pointing at a host temp path. The shared file is the
    // only one the sandbox can read.
    env.insert(
        rhema_bash::init::VAR_BASH_ENV.to_owned(),
        sandbox_session
            .join("init.sh")
            .to_string_lossy()
            .into_owned(),
    );
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
    let tag = env
        .get("RHEMA_SESSION")
        .cloned()
        .unwrap_or_else(|| session.to_owned());
    std::fs::write(host_session.join("jobs/session"), tag)?;
    Ok(Placed {
        env,
        init,
        helper,
        skipped: Vec::new(),
        session: SessionDir { path: host_session },
    })
}

/// The session age after which a start sweep removes a tree, the
/// spec's seven days: a live session is much younger, a killed host's
/// leftovers are much older.
const SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The install age after which a start sweep removes a version, the
/// spec's 30 days. The running version is never removed.
const INSTALL_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Removes a crashed host's leftovers: session trees older than seven
/// days, and helper versions older than 30 days other than this
/// process's. One level deep, no symlinks, errors ignored.
pub fn sweep(shared: &SharedRoot) {
    sweep_dir(&shared.host.join("sessions"), SESSION_MAX_AGE, None);
    sweep_dir(
        &shared.host.join("bin"),
        INSTALL_MAX_AGE,
        Some(&KIT_VERSION.to_string()),
    );
}

fn sweep_dir(dir: &Path, max_age: Duration, keep: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep.is_some_and(|keep| name.to_str() == Some(keep)) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age > max_age) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Runs the sandbox-side installer once, records the version, and
/// probes the result. The install directory is `bin/<version>` under
/// the shared root, the tree both sides see.
///
/// # Errors
///
/// The installer fails, the directory cannot be written, or the
/// installed helper does not answer this host's [`KIT_VERSION`].
pub async fn install(
    sandbox: &dyn crate::Sandbox,
    shared: &SharedRoot,
    installer: &Installer,
) -> Result<(), KitError> {
    let version = KIT_VERSION.to_string();
    let dir = shared.install_dir(&version);
    std::fs::create_dir_all(&dir)?;
    let sandbox_dir = Path::new(&shared.sandbox).join("bin").join(&version);
    run_command(
        sandbox,
        installer.for_dir(&sandbox_dir.to_string_lossy()),
        "the kit installer",
    )
    .await?;
    std::fs::write(dir.join(".kit-version"), &version)?;
    verify(sandbox, shared).await
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
    let version = KIT_VERSION.to_string();
    let helper = Path::new(&shared.sandbox)
        .join("bin")
        .join(&version)
        .join("rhema");
    let outcome = run_command(
        sandbox,
        format!("{} __kit-version", shquote(&helper.to_string_lossy())),
        "the helper's version probe",
    )
    .await?;
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
    fn the_shared_placement_writes_the_init_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedRoot::new(dir.path());
        let kit = Kit::new(
            Feature::Jobs,
            vec![Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap()],
            Needs::MACRO,
        );
        let placed = place_shared(&shared, "s1", std::slice::from_ref(&kit)).unwrap();
        let env = placed.env;
        let init = env.get("BASH_ENV").expect("init");
        assert_eq!(
            init,
            &dir.path()
                .join("sessions/s1/init.sh")
                .to_string_lossy()
                .into_owned()
        );
        assert!(Path::new(init).is_file());
        let text = std::fs::read_to_string(init).unwrap();
        assert!(
            text.starts_with(&format!(
                "export PATH='{}':",
                dir.path().join(format!("bin/{KIT_VERSION}")).display()
            )),
            "the helper's dir rides PATH for gob: {text}"
        );
        assert_eq!(
            env.get("RHEMA_JOBS_BIN").map(String::as_str),
            Some(
                dir.path()
                    .join(format!("bin/{KIT_VERSION}/rhema"))
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(dir.path().join("sessions/s1/jobs/session").is_file());
    }

    #[test]
    fn a_kit_init_never_replaces_the_shared_one() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedRoot::new(dir.path());
        let mut kit = kit(Feature::Search, Needs::HOST_CALL);
        kit.vars
            .insert("BASH_ENV".into(), "/host/tmp/init.bash".into());
        kit.vars.insert("RHEMA_SEARCH_CHAIN".into(), "[]".into());
        let placed = place_shared(&shared, "s1", std::slice::from_ref(&kit)).unwrap();
        let env = placed.env;
        assert_eq!(
            env.get("BASH_ENV").map(String::as_str),
            Some(
                dir.path()
                    .join("sessions/s1/init.sh")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            env.get("RHEMA_SEARCH_CHAIN").map(String::as_str),
            Some("[]")
        );
    }

    #[test]
    fn a_placed_session_removes_its_tree_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedRoot::new(dir.path());
        let kit = kit(Feature::Jobs, Needs::MACRO);
        let placed = place_shared(&shared, "s1", std::slice::from_ref(&kit)).unwrap();
        let tree = dir.path().join("sessions/s1");
        assert!(tree.is_dir());
        drop(placed);
        assert!(!tree.exists(), "the guard owns the session tree");
    }

    #[cfg(unix)]
    fn age(path: &Path, days: u64) {
        let file = std::fs::File::open(path).unwrap();
        let when = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_start_sweep_removes_stale_state_and_keeps_the_live_version() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedRoot::new(dir.path());
        let stale = shared.session_dir("old");
        let fresh = shared.session_dir("fresh");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        age(&stale, 8);
        let old_bin = shared.host.join("bin/1");
        let current_bin = shared.install_dir(&KIT_VERSION.to_string());
        std::fs::create_dir_all(&old_bin).unwrap();
        std::fs::create_dir_all(&current_bin).unwrap();
        age(&old_bin, 31);

        sweep(&shared);

        assert!(!stale.exists(), "a stale session goes");
        assert!(fresh.exists(), "a fresh session stays");
        assert!(!old_bin.exists(), "a stale version goes");
        assert!(current_bin.exists(), "the running version stays");
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

    #[tokio::test]
    async fn a_shared_placement_installs_and_generates() {
        use crate::mock::ScriptedSandbox;
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedRoot::new(dir.path().join("runtime"));
        let kit = Kit::new(
            Feature::Jobs,
            vec![Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap()],
            Needs::MACRO,
        );
        let sandbox = ScriptedSandbox::new();
        for outcome in [
            crate::RunOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                code: Some(0),
            },
            crate::RunOutcome {
                stdout: format!("{KIT_VERSION}\n").into_bytes(),
                stderr: Vec::new(),
                code: Some(0),
            },
            crate::RunOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                code: Some(0),
            },
        ] {
            sandbox.push_run(Ok(outcome));
        }
        let installer = Installer::new("install-rhema {dir}");
        let caps = Capabilities::mounted(shared.clone());
        let placed = ensure_shared(&sandbox, &shared, "s1", &installer, &[kit], &caps)
            .await
            .unwrap();
        assert!(
            placed.env["BASH_ENV"].ends_with("/sessions/s1/init.sh"),
            "{:?}",
            placed.env
        );
        assert!(
            placed.env["RHEMA_JOBS_BIN"].ends_with(&format!("/bin/{KIT_VERSION}/rhema")),
            "{:?}",
            placed.env
        );
        assert_eq!(placed.env["RHEMA_SESSION"], "s1");
        assert!(
            shared
                .install_dir(&KIT_VERSION.to_string())
                .join(".kit-version")
                .is_file()
        );
        let commands = sandbox.executed();
        assert!(commands[0].contains("install-rhema"), "{commands:?}");
        assert!(commands[1].contains("__kit-version"), "{commands:?}");
        assert!(commands[2].contains("__init"), "{commands:?}");
        assert!(commands[2].contains("--stub"), "{commands:?}");
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
            stdout: format!("{KIT_VERSION}\n").into_bytes(),
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
            stdout: b"999\n".to_vec(),
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
