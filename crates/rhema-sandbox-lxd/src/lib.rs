//! The LXD backend: one driver, two postures, both through the `lxc`
//! CLI (spec §4's "LXD / Incus" row).
//!
//! [`LxdSandbox::mounted`] is the fs-sharing family's LXD member: a
//! container whose `disk` device mirrors the host workspace 1:1 — the
//! docker bind-mount posture — so the file routes are plain fs through
//! [`FsRoot`] and only exec crosses the CLI. [`LxdSandbox::attached`]
//! is the fs-disjoint one: no mount, files crossing `lxc file push`
//! and `pull`, the workspace carried in by the seed. That is a
//! virtual machine's posture (`lxc launch --vm` takes no host-path
//! mount) and the fallback for any instance left unmounted.
//!
//! Containers and `--vm` instances exec identically — `lxc exec`
//! speaks the same argv to both — which is why the instance type is a
//! launch flag on the assembly, not a split in the choice vocabulary
//! (spec §9). The adapter never launches: the caller stands the
//! instance up (`lxc launch [--vm] <image> <name>`) and adds the disk
//! device, the same division as the docker backend.
//!
//! Exec is `lxc exec [--user <uid>] [--group <gid>] [--cwd <dir>]
//! <instance> -- timeout --kill-after=5 <secs> bash -c <command>`.
//! The user flags are the workspace owner's, given by the caller
//! ([`LxdSandbox::with_user`]): an unprivileged container maps uids,
//! so commands must run as the uid the host tree belongs to — as
//! root they touch nobody's files and, privileged, they would touch
//! everyone's. The cwd defaults to the workspace: the exec lands
//! where the session lives, not in the instance's root. The argv
//! rides the exec API to the instance's process intact — no shell
//! reparse, the ssh lesson inverted (sshd joins argv into one shell
//! line; LXD does not) — so nothing here is quoted for a middle
//! shell. The kill is
//! the prefix's, as on every backend without a per-command kill —
//! the `timeout` prefix's codes (124 at the budget, an external SIGKILL reads as a plain failure) read through `rhema-sandbox`'s shared `exit_status`. Streams pump live lines through
//! the shared `run_child` wiring (`exec_util`), caps included.
//!
//! Cancelling mid-run kills the local `lxc` process only; the command
//! keeps running inside the instance, and whatever it wrote through
//! the mount — or wherever it got in the attached posture — is there
//! afterwards. The local socket is the whole transport: no
//! `remote:` prefixes, the CLI's remote syntax stays out of the
//! constructor's contract.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

use rhema_sandbox::exec_util::{
    self, STAGING, check_remote_path, read_through, under_workspace, write_through,
};
use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The seed budget for both seed kinds, the shim's and the siblings'.
const SEED_BUDGET: Duration = Duration::from_secs(300);

/// The budget of one attached fs call (`lxc file pull` / `push`): the
/// exec routes carry their request's budget, the fs routes have none
/// to carry, and a black-holed transport would hang `read_file` or
/// `write_file` forever without a fixed one.
const FS_BUDGET: Duration = Duration::from_secs(60);

/// The LXD sandbox a session's commands run in: one instance —
/// container or virtual machine, the adapter never cares — driven
/// through the `lxc` CLI.
#[derive(Debug, Clone)]
pub struct LxdSandbox {
    /// The instance's name, used verbatim on every `lxc` command line.
    instance: String,
    /// The host directory standing in as the sandbox filesystem — the
    /// host side of the instance's `disk` device. `Some` is the
    /// mounted posture (fs-sharing family); `None` is the attached
    /// one, whose file routes cross `lxc file`.
    root: Option<FsRoot>,
    /// The instance-side path of the workspace. Mounted, it is where
    /// the device maps the host root (mirrored 1:1, so instance and
    /// host paths agree); attached, it is where the seed carries the
    /// workspace in.
    workspace: String,
    /// The uid/gid exec runs as — the workspace owner's, so the
    /// instance's commands touch the tree with the host's own
    /// identity. Unset, exec runs as the instance's root.
    user: Option<(u32, u32)>,
}

impl LxdSandbox {
    /// A sandbox in an instance whose `disk` device maps the host root
    /// `root` at `workspace` inside — the docker bind-mount posture:
    ///
    /// ```text
    /// lxc launch ubuntu:24.04 rhema-sandbox
    /// lxc config device add rhema-sandbox workspace disk \
    ///     source=/home/me/proj path=/home/me/proj
    /// let root = FsRoot::new(PathBuf::from("/home/me/proj"))?;
    /// let sandbox = LxdSandbox::mounted("rhema-sandbox", root, "/home/me/proj");
    /// ```
    ///
    /// Sandbox paths are mount-relative or instance-side —
    /// `/src/main.rs` and `<workspace>/src/main.rs` both name
    /// `<root>/src/main.rs` on the host and `<workspace>/src/main.rs`
    /// in the instance, and exec's working directory is the
    /// instance-side path of the sandbox cwd either way.
    pub fn mounted(
        instance: impl Into<String>,
        root: FsRoot,
        workspace: impl Into<String>,
    ) -> Self {
        Self {
            instance: instance.into(),
            root: Some(root),
            workspace: workspace.into(),
            user: None,
        }
    }

    /// A sandbox in an instance taken as it stands — no mount, the
    /// file routes crossing `lxc file`, the workspace carried in by
    /// the seed. A virtual machine's posture. [`Self::with_workspace`]
    /// names where the seed lands it.
    pub fn attached(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            root: None,
            workspace: String::new(),
            user: None,
        }
    }

    /// Names the instance-side workspace the seed carries the tree
    /// into. Attached only — the mounted posture's workspace rides the
    /// constructor.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    /// Names the uid/gid exec runs as. Mounted, this is the host
    /// workspace owner's — an unprivileged container's map makes that
    /// the only identity the tree answers to. The assembly stats the
    /// workspace and passes the owner through.
    pub fn with_user(mut self, uid: u32, gid: u32) -> Self {
        self.user = Some((uid, gid));
        self
    }

    /// Runs one seed command on the host, at the root the instance's
    /// device mirrors: mounted-only, and plain fs like docker's. The
    /// budget and kill are the same `timeout` prefix every exec gets;
    /// a nonzero exit surfaces the stderr tail.
    async fn run_host_seed(&self, argv: &[&str]) -> Result<(), SandboxError> {
        let root = self
            .root
            .as_ref()
            .expect("run_host_seed is mounted-only")
            .map("/")?;
        let output = tokio::process::Command::new("timeout")
            .arg("--kill-after=5")
            .arg(SEED_BUDGET.as_secs().to_string())
            .args(argv)
            .current_dir(root)
            .output()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        // The prefix's own codes decide: budget expiry, or the
        // external kill that must not read as one.
        rhema_sandbox::exec_util::exit_status(output.status.code(), SEED_BUDGET.as_secs())?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            return Err(SandboxError::Failed(if detail.is_empty() {
                "seed command failed".into()
            } else {
                detail.to_owned()
            }));
        }
        Ok(())
    }

    /// Runs one instance-side seed command — attached-only — under the
    /// seed budget: past it the local `lxc` is dropped and the budget
    /// reads as a timeout; a nonzero exit surfaces the stderr tail.
    async fn run_attached_seed(&self, argv: &[String]) -> Result<(), SandboxError> {
        let mut full = vec!["lxc".to_owned(), "exec".to_owned(), self.instance.clone()];
        full.extend(user_flags(self.user));
        full.extend([
            "--".to_owned(),
            "timeout".to_owned(),
            "--kill-after=5".to_owned(),
            SEED_BUDGET.as_secs().to_string(),
        ]);
        full.extend(argv.iter().cloned());
        match tokio::time::timeout(SEED_BUDGET, self.capture(&full)).await {
            Err(_) => Err(SandboxError::Timeout(SEED_BUDGET.as_secs())),
            Ok(answer) => {
                let (stdout, code, stderr) = answer?;
                let _ = stdout;
                match code {
                    Some(0) => Ok(()),
                    code => Err(lxd_failure(code, &stderr)),
                }
            }
        }
    }

    /// Spawns one local `lxc` carrying `argv`, answering the stdout
    /// bytes, the exit code, and the stderr text — the file routes'
    /// and the attached seed's one-shot shape.
    /// One lxc call under a wall-clock budget: past it the future
    /// drops, and with it the local lxc (`kill_on_drop`) — a
    /// black-holed transport reads as a timeout, not a hang.
    async fn budgeted<T>(
        &self,
        budget: Duration,
        call: impl std::future::Future<Output = Result<T, SandboxError>>,
    ) -> Result<T, SandboxError> {
        match tokio::time::timeout(budget, call).await {
            Ok(answer) => answer,
            Err(_) => Err(SandboxError::Timeout(budget.as_secs())),
        }
    }

    async fn capture(
        &self,
        argv: &[String],
    ) -> Result<(Vec<u8>, Option<i32>, String), SandboxError> {
        use std::process::Stdio;

        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A route past its patience drops this future; the drop
            // kills the local lxc.
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        Ok((
            output.stdout,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// The instance-side path of the sandbox cwd. Mounted, sandbox paths
/// are mount-relative, so the instance path is `workspace` plus the
/// cwd; attached, the sandbox path is already instance-side. `None`
/// for the unset cwd (`""`) — `lxc exec` keeps its own default
/// working directory.
fn instance_cwd(mounted: bool, workspace: &str, cwd: &str) -> Option<String> {
    // An empty cwd is the workspace itself: the exec lands where the
    // session lives, not in the instance's default directory. No
    // workspace named — the attached sandbox without one — and lxc's
    // own default stands.
    if cwd.is_empty() {
        return if workspace.is_empty() {
            None
        } else {
            Some(workspace.to_owned())
        };
    }
    if !mounted {
        return Some(cwd.to_owned());
    }
    let base = workspace.trim_end_matches('/');
    let tail = cwd.trim_end_matches('/');
    Some(match (base.is_empty(), tail.is_empty()) {
        (true, true) => "/".to_owned(),
        (true, false) => tail.to_owned(),
        (false, true) => base.to_owned(),
        (false, false) => format!("{base}{tail}"),
    })
}

/// The exec argv: `lxc exec [--cwd <instance cwd>] <instance> --
/// timeout --kill-after=5 <secs> bash -c <command>`. `lxc exec` has
/// its own `--cwd`, preferred over the shim's `cd` prefix — cleaner,
/// and no shell quoting for the directory. Nothing after `--` is
/// quoted: the argv rides the exec API intact, no shell reparse.
fn exec_argv(
    instance: &str,
    mounted: bool,
    workspace: &str,
    timeout_secs: u64,
    cwd: &str,
    command: &str,
    user: Option<(u32, u32)>,
) -> Vec<String> {
    let mut argv = vec!["lxc".to_owned(), "exec".to_owned()];
    if let Some(dir) = instance_cwd(mounted, workspace, cwd) {
        argv.push("--cwd".to_owned());
        argv.push(dir);
    }
    argv.extend(user_flags(user));
    argv.extend([
        instance.to_owned(),
        "--".to_owned(),
        "timeout".to_owned(),
        "--kill-after=5".to_owned(),
        timeout_secs.to_string(),
        "bash".to_owned(),
        "-c".to_owned(),
        command.to_owned(),
    ]);
    argv
}

/// The exec identity: `--user <uid> --group <gid>`, the workspace
/// owner's, so instance commands touch the tree as the host user —
/// not as nobody's-root in an unprivileged map, not as everyone's
/// root in a privileged one.
fn user_flags(user: Option<(u32, u32)>) -> Vec<String> {
    match user {
        None => Vec::new(),
        Some((uid, gid)) => vec![
            "--user".to_owned(),
            uid.to_string(),
            "--group".to_owned(),
            gid.to_string(),
        ],
    }
}

/// The read argv, attached: `lxc file pull -- <instance><path> -` —
/// the `-` destination is the local stdout. Instance and path
/// concatenate; instance-side paths are absolute, so the seam is
/// unambiguous.
fn read_argv(instance: &str, path: &str) -> Vec<String> {
    vec![
        "lxc".to_owned(),
        "file".to_owned(),
        "pull".to_owned(),
        "--".to_owned(),
        format!("{instance}{path}"),
        "-".to_owned(),
    ]
}

/// The write argv, attached:
/// `lxc file push --create-dirs -- <staged> <instance><path>` — the
/// bytes ride in a host temp file, and `--create-dirs` makes the
/// instance-side parents, the shim's put semantics.
fn write_argv(instance: &str, staged: &str, path: &str) -> Vec<String> {
    vec![
        "lxc".to_owned(),
        "file".to_owned(),
        "push".to_owned(),
        "--create-dirs".to_owned(),
        "--".to_owned(),
        staged.to_owned(),
        format!("{instance}{path}"),
    ]
}

/// The attached git-seed argv: `lxc exec <instance> -- git clone
/// <url> <workspace>` — the repository becomes the instance-side
/// workspace, git making the directory. Direct argv, no shell, no
/// quoting: the pieces ride the exec API intact.
fn clone_argv(instance: &str, url: &str, workspace: &str) -> Vec<String> {
    vec![
        "lxc".to_owned(),
        "exec".to_owned(),
        instance.to_owned(),
        "--".to_owned(),
        "git".to_owned(),
        "clone".to_owned(),
        url.to_owned(),
        workspace.to_owned(),
    ]
}

/// The attached tarball's extract argv, run after the tarball is
/// pushed through the write route: `lxc exec <instance> -- tar -xzf
/// <workspace>/<staging> -C <workspace>`.
fn extract_argv(instance: &str, workspace: &str) -> Vec<String> {
    vec![
        "lxc".to_owned(),
        "exec".to_owned(),
        instance.to_owned(),
        "--".to_owned(),
        "tar".to_owned(),
        "-xzf".to_owned(),
        format!("{workspace}/{STAGING}"),
        "-C".to_owned(),
        workspace.to_owned(),
    ]
}

/// Why a one-shot `lxc` call failed: the stderr tail is the message,
/// a silent exit falls back to the code.
fn lxd_failure(code: Option<i32>, stderr: &str) -> SandboxError {
    let detail = stderr.trim();
    SandboxError::Failed(if detail.is_empty() {
        format!("lxc exited with {code:?}")
    } else {
        detail.to_owned()
    })
}

#[async_trait]
impl Sandbox for LxdSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        use std::process::Stdio;
        use std::sync::atomic::Ordering;

        // The trait's first observation point: a flip before the start
        // never reaches the instance.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        // The cwd is the instance's to interpret, not this process's
        // fs: the textual guard is what stands between the model and a
        // `..` climb, the same guard the fs-disjoint backends lean on.
        check_remote_path(&request.cwd)?;
        // The mounted caller names paths instance-side (the mirrored
        // workspace's own path); the workdir join wants mount-relative
        // ones. Attached, the paths are already instance-side.
        let cwd = match &self.root {
            Some(_) => under_workspace(&self.workspace, &request.cwd),
            None => request.cwd.clone(),
        };
        let argv = exec_argv(
            &self.instance,
            self.root.is_some(),
            &self.workspace,
            request.timeout.as_secs(),
            &cwd,
            &request.command,
            self.user,
        );
        let child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The explicit kill below is the mechanism on cancel; the
            // drop is the backstop for the early returns.
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        exec_util::run_child(child, &request, cancelled, progress).await
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        match seed {
            WorkspaceSeed::GitClone(url) => match &self.root {
                // Mounted: the host-side clone, plain fs through the
                // root the device mirrors.
                Some(_) => self.run_host_seed(&["git", "clone", url, "."]).await,
                None => {
                    let ws = if self.workspace.is_empty() {
                        return Err(SandboxError::Failed(
                            "the attached lxd sandbox needs a workspace: with_workspace names \
                             where the seed lands"
                                .into(),
                        ));
                    } else {
                        self.workspace.clone()
                    };
                    self.run_attached_seed(&clone_argv(&self.instance, url, &ws))
                        .await
                }
            },
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                // Staged through the file route, unpacked at the root.
                // Mounted, both halves are plain fs on the host side of
                // the device (the staging file lands at the root);
                // attached, the push makes the instance-side workspace
                // directory and the extract runs inside.
                let (staging, unpack) = match &self.root {
                    Some(_) => (format!("/{STAGING}"), None),
                    None => {
                        if self.workspace.is_empty() {
                            return Err(SandboxError::Failed(
                                "the attached lxd sandbox needs a workspace: with_workspace names \
                                 where the seed lands"
                                    .into(),
                            ));
                        }
                        (
                            format!("{}/{STAGING}", self.workspace),
                            Some(extract_argv(&self.instance, &self.workspace)),
                        )
                    }
                };
                self.write_file(&staging, &bytes).await?;
                let unpacked = match &unpack {
                    Some(argv) => self.run_attached_seed(argv).await,
                    None => self.run_host_seed(&["tar", "-xzf", STAGING]).await,
                };
                // The staging file is not workspace content; a leftover
                // one is harmless, so its removal is best-effort.
                if let Some(root) = &self.root {
                    if let Ok(staged) = root.map(&format!("/{STAGING}")) {
                        let _ = tokio::fs::remove_file(staged).await;
                    }
                } else {
                    let _ = self
                        .capture(&[
                            "lxc".to_owned(),
                            "exec".to_owned(),
                            self.instance.clone(),
                            "--".to_owned(),
                            "rm".to_owned(),
                            "-f".to_owned(),
                            staging.clone(),
                        ])
                        .await;
                }
                unpacked
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        match &self.root {
            // The mount is the route: plain fs on the host side, the
            // instance-side path translated to mount-relative first.
            Some(root) => read_through(root, &under_workspace(&self.workspace, path)).await,
            None => {
                check_remote_path(path)?;
                let (stdout, code, stderr) = self
                    .budgeted(FS_BUDGET, self.capture(&read_argv(&self.instance, path)))
                    .await?;
                match code {
                    Some(0) => Ok(stdout),
                    code => Err(lxd_failure(code, &stderr)),
                }
            }
        }
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        match &self.root {
            Some(root) => {
                write_through(root, &under_workspace(&self.workspace, path), contents).await
            }
            None => {
                check_remote_path(path)?;
                // The bytes stage in a host temp file — `lxc file
                // push` takes a path, not a pipe — and the staging
                // file's life ends with the push.
                let staged = tempfile::Builder::new()
                    .prefix("rhema-lxd-")
                    .tempfile()
                    .map_err(|err| SandboxError::Failed(format!("the staging file: {err}")))?;
                {
                    use std::io::Write;
                    let mut file = staged.as_file();
                    file.write_all(contents)
                        .map_err(|err| SandboxError::Failed(format!("the staging file: {err}")))?;
                    file.flush()
                        .map_err(|err| SandboxError::Failed(format!("the staging file: {err}")))?;
                }
                let stage_path = staged.path().to_string_lossy().into_owned();
                let (_, code, stderr) = self
                    .budgeted(
                        FS_BUDGET,
                        self.capture(&write_argv(&self.instance, &stage_path, path)),
                    )
                    .await?;
                staged.close().map_err(|err| {
                    SandboxError::Failed(format!("the staging file would not close: {err}"))
                })?;
                match code {
                    Some(0) => Ok(()),
                    code => Err(lxd_failure(code, &stderr)),
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A mounted sandbox over a fresh tempdir standing in for the host
    /// side of the device; the guard comes back so the root outlives
    /// the test.
    fn mounted() -> (LxdSandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        (
            LxdSandbox::mounted("rhema-sandbox-test", root, "/home/me/proj"),
            dir,
        )
    }

    #[test]
    fn the_exec_argv_carries_the_cwd_and_the_timeout_prefix() {
        assert_eq!(
            exec_argv(
                "box",
                true,
                "/home/me/proj",
                30,
                "/src",
                "echo hi",
                Some((1000, 1000)),
            ),
            vec![
                "lxc",
                "exec",
                "--cwd",
                "/home/me/proj/src",
                "--user",
                "1000",
                "--group",
                "1000",
                "box",
                "--",
                "timeout",
                "--kill-after=5",
                "30",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn an_unset_cwd_lands_at_the_workspace() {
        assert_eq!(
            exec_argv(
                "box",
                true,
                "/home/me/proj",
                30,
                "",
                "echo hi",
                Some((1000, 1000)),
            ),
            vec![
                "lxc",
                "exec",
                "--cwd",
                "/home/me/proj",
                "--user",
                "1000",
                "--group",
                "1000",
                "box",
                "--",
                "timeout",
                "--kill-after=5",
                "30",
                "bash",
                "-c",
                "echo hi",
            ]
        );
        // No workspace named either: nothing to fall back to, lxc's
        // own default stands.
        assert_eq!(
            exec_argv("vm0", false, "", 30, "", "echo hi", None),
            vec![
                "lxc",
                "exec",
                "vm0",
                "--",
                "timeout",
                "--kill-after=5",
                "30",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn an_attached_cwd_is_taken_as_instance_side() {
        assert_eq!(
            exec_argv("vm0", false, "", 30, "/src", "echo hi", None),
            vec![
                "lxc",
                "exec",
                "--cwd",
                "/src",
                "vm0",
                "--",
                "timeout",
                "--kill-after=5",
                "30",
                "bash",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn the_root_cwd_workdirs_at_the_mount_itself() {
        assert_eq!(
            instance_cwd(true, "/home/me/proj", "/"),
            Some("/home/me/proj".to_owned())
        );
        // The empty cwd is the workspace, not the instance's default:
        // the exec lands where the session lives.
        assert_eq!(
            instance_cwd(true, "/home/me/proj", ""),
            Some("/home/me/proj".to_owned())
        );
    }

    #[test]
    fn the_workspace_absolute_cwd_joins_without_doubling() {
        // The session's cwd names the workspace itself, so the join
        // must not repeat the mount prefix.
        assert_eq!(
            instance_cwd(
                true,
                "/home/me/proj",
                &under_workspace("/home/me/proj", "/home/me/proj/src"),
            ),
            Some("/home/me/proj/src".to_owned())
        );
        assert_eq!(
            instance_cwd(
                true,
                "/home/me/proj",
                &under_workspace("/home/me/proj", "/home/me/proj"),
            ),
            Some("/home/me/proj".to_owned())
        );
    }

    #[tokio::test]
    async fn a_workspace_absolute_file_route_lands_at_the_mount_root() {
        let (sandbox, dir) = mounted();
        sandbox
            .write_file("/home/me/proj/.rhema/.seeded", b"1")
            .await
            .expect("the write lands");
        assert_eq!(
            std::fs::read(dir.path().join(".rhema/.seeded")).expect("the file is at the root"),
            b"1"
        );
        assert_eq!(
            sandbox
                .read_file("/home/me/proj/.rhema/.seeded")
                .await
                .expect("the read lands"),
            b"1"
        );
    }

    #[test]
    fn the_file_route_argvs_name_the_seam() {
        assert_eq!(
            read_argv("box", "/tmp/note.md"),
            vec!["lxc", "file", "pull", "--", "box/tmp/note.md", "-",]
        );
        assert_eq!(
            write_argv("box", "/tmp/rhema-lxd-staged", "/tmp/note.md"),
            vec![
                "lxc",
                "file",
                "push",
                "--create-dirs",
                "--",
                "/tmp/rhema-lxd-staged",
                "box/tmp/note.md",
            ]
        );
        assert_eq!(
            clone_argv("box", "https://example.com/repo", "/home/me/proj"),
            vec![
                "lxc",
                "exec",
                "box",
                "--",
                "git",
                "clone",
                "https://example.com/repo",
                "/home/me/proj",
            ]
        );
        assert_eq!(
            extract_argv("box", "/home/me/proj"),
            vec![
                "lxc",
                "exec",
                "box",
                "--",
                "tar",
                "-xzf",
                "/home/me/proj/.rhema-seed.tar.gz",
                "-C",
                "/home/me/proj",
            ]
        );
    }

    #[tokio::test]
    async fn a_relative_cwd_is_refused_without_spawning_lxc() {
        let (sandbox, _guard) = mounted();
        for cwd in ["src", "src/../.."] {
            let err = sandbox
                .exec(
                    ExecRequest {
                        command: "echo hi".into(),
                        cwd: cwd.to_owned(),
                        timeout: Duration::from_secs(30),
                    },
                    &AtomicBool::new(false),
                    &mut |_| {},
                )
                .await
                .expect_err("the guard wins");
            assert!(matches!(err, SandboxError::Failed(ref message) if message.contains(cwd)));
        }
    }

    #[tokio::test]
    async fn a_flip_before_the_start_never_reaches_the_instance() {
        let (sandbox, _guard) = mounted();
        let cancelled = AtomicBool::new(true);
        let err = sandbox
            .exec(
                ExecRequest {
                    command: "echo hi".into(),
                    cwd: "/".into(),
                    timeout: Duration::from_secs(30),
                },
                &cancelled,
                &mut |_| {},
            )
            .await
            .expect_err("the flip wins");
        assert!(matches!(err, SandboxError::Failed(ref message) if message == "cancelled"));
    }

    #[tokio::test]
    async fn files_round_trip_through_the_mapped_root() {
        let (sandbox, _guard) = mounted();
        sandbox
            .write_file("/src/main.rs", b"fn main() {}\n")
            .await
            .expect("the write lands");
        let read = sandbox
            .read_file("/src/main.rs")
            .await
            .expect("the read answers");
        assert_eq!(read, b"fn main() {}\n");
    }

    #[tokio::test]
    async fn a_missing_file_read_fails_in_band() {
        let (sandbox, _guard) = mounted();
        let err = sandbox
            .read_file("/nope")
            .await
            .expect_err("the miss is in-band");
        assert!(err.to_string().contains("/nope"));
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A one-commit repository with one file, for the clone seed.
    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "."]);
        std::fs::write(dir.path().join("seeded.txt"), "from git").expect("the file lands");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "seed"]);
        dir
    }

    /// GNU `timeout` is a Linux tool: a machine without it in PATH
    /// cannot exercise the seed route's prefix, and these tests skip.
    fn have_timeout() -> bool {
        std::process::Command::new("timeout")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn seeding_from_a_repository_populates_the_mounted_root() {
        if !have_timeout() {
            eprintln!("skipping: no GNU timeout on this machine");
            return;
        }
        let repo = fixture_repo();
        let (sandbox, _guard) = mounted();
        sandbox
            .seed(&WorkspaceSeed::GitClone(
                repo.path().to_string_lossy().into_owned(),
            ))
            .await
            .expect("the host clone lands");
        let read = sandbox
            .read_file("/seeded.txt")
            .await
            .expect("the cloned file reads back");
        assert_eq!(read, b"from git");
    }

    #[tokio::test]
    async fn a_failed_clone_fails_in_band() {
        if !have_timeout() {
            eprintln!("skipping: no GNU timeout on this machine");
            return;
        }
        let (sandbox, _guard) = mounted();
        let err = sandbox
            .seed(&WorkspaceSeed::GitClone("/definitely/not/a/repo".into()))
            .await
            .expect_err("the miss is in-band");
        assert!(matches!(err, SandboxError::Failed(_)));
    }

    #[tokio::test]
    async fn seeding_from_a_tarball_unpacks_into_the_mounted_root() {
        if !have_timeout() {
            eprintln!("skipping: no GNU timeout on this machine");
            return;
        }
        let (sandbox, dir) = mounted();
        // A gzipped tarball built the ordinary way, at the staging
        // name the seed unpacks from.
        std::fs::write(dir.path().join("note.txt"), "from tarball").expect("the file lands");
        let tar = std::process::Command::new("tar")
            .args(["-czf", "-", "note.txt"])
            .current_dir(dir.path())
            .stdout(Stdio::piped())
            .output()
            .expect("tar runs");
        assert!(tar.status.success());
        use base64::Engine as _;
        sandbox
            .seed(&WorkspaceSeed::Tarball {
                name: "seed.tar.gz".into(),
                tar_gz_base64: base64::engine::general_purpose::STANDARD.encode(&tar.stdout),
            })
            .await
            .expect("the tarball seed lands");
        let read = sandbox
            .read_file("/note.txt")
            .await
            .expect("the unpacked file reads back");
        assert_eq!(read, b"from tarball");
        assert!(
            !dir.path().join(".rhema-seed.tar.gz").exists(),
            "the staging file is cleaned up"
        );
    }

    // --- The live section: a real `lxc` answering on the local socket.

    /// Whether a local LXD answers at all: `lxc info` exiting zero
    /// inside the probe budget. Without it the live tests stand down.
    fn lxd_available() -> bool {
        std::process::Command::new("lxc")
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn exec_runs_a_command_inside_the_live_instance() {
        if !lxd_available() {
            return;
        }
        // The caller stands the instance up; the test picks a name
        // `lxc list` is unlikely to carry and reads the truth back.
        let instance = "rhema-lxd-live-test";
        if !std::process::Command::new("lxc")
            .args(["list", instance, "--format", "csv"])
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
        {
            return;
        }
        let sandbox = LxdSandbox::attached(instance).with_workspace("/root/proj");
        let outcome = run(
            &sandbox,
            ExecRequest {
                command: "echo hello-from-lxd".into(),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),
            },
        )
        .await;
        assert!(
            outcome
                .stdout
                .windows("hello-from-lxd".len())
                .any(|window| window == "hello-from-lxd".as_bytes()),
            "the live exec answers: {:?}",
            outcome
        );
    }

    /// One exec, answer collected.
    async fn run(sandbox: &LxdSandbox, request: ExecRequest) -> ExecOutcome {
        let cancelled = AtomicBool::new(false);
        sandbox
            .exec(request, &cancelled, &mut |_| {})
            .await
            .expect("the exec answers")
    }
}
