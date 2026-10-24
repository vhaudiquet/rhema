//! The docker backend: a bind-mounted container driven through the
//! `docker` CLI.
//!
//! [`DockerSandbox`] is the fs-sharing family's container member (spec
//! §4's "container, v1" row, the bind-mount posture): `FsRoot` plus a
//! `docker exec` prefix, no Engine API. The kill is the prefix's, as
//! on every backend without a per-command kill —
//! `timeout --kill-after=5` rides inside the container, `ExecRequest::
//! timeout` stays the one budget knob, and exits 124/137 read as a
//! timeout. Streams pump live lines through the shared `run_child`
//! wiring (`exec_util`), caps included.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

use rhema_sandbox::exec_util::{self, STAGING, check_remote_path};
use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The docker sandbox a session's commands run in: one running
/// container whose bind mount is the sandbox filesystem.
///
/// The caller stands the container up and bind-mounts the host
/// directory — the one [`FsRoot`] maps sandbox paths onto — at
/// `workspace` inside the container:
///
/// ```text
/// docker run -d --name rhema-sandbox \
///     -v /home/me/proj:/workspace debian:trixie sleep infinity
/// let root = FsRoot::new(PathBuf::from("/home/me/proj"))?;
/// let sandbox = DockerSandbox::new("rhema-sandbox", root, "/workspace");
/// ```
///
/// That mount is the posture's whole point: the file routes are plain
/// fs through [`FsRoot`] on the host, and the container sees every
/// change live. Sandbox paths are mount-relative — sandbox path
/// `/src/main.rs` is `<root>/src/main.rs` on the host and
/// `/workspace/src/main.rs` in the container — and exec's working
/// directory is the container-side path of the sandbox cwd.
///
/// Cancelling mid-run kills the local `docker exec` process only; the
/// command keeps running inside the container, and whatever it wrote
/// through the mount is there afterwards.
#[derive(Debug, Clone)]
pub struct DockerSandbox {
    /// The running container's name, used verbatim on every `docker`
    /// command line.
    container: String,
    /// The host directory standing in as the sandbox filesystem — the
    /// host side of the container's bind mount.
    root: FsRoot,
    /// The path the host root is mounted at inside the container.
    workspace: String,
}

impl DockerSandbox {
    /// A sandbox in `container`, whose host root `root` is mounted at
    /// `workspace` inside the container.
    pub fn new(container: impl Into<String>, root: FsRoot, workspace: impl Into<String>) -> Self {
        Self {
            container: container.into(),
            root,
            workspace: workspace.into(),
        }
    }

    /// Runs one seed command on the host, at the root the container
    /// bind-mounts: the seeding is plain fs through the [`FsRoot`] and
    /// needs nothing of the container but the mount. The budget and
    /// kill are the same `timeout` prefix every exec gets; a nonzero
    /// exit surfaces the stderr tail, as the shim's seed route reports
    /// a failed clone or unpack.
    async fn run_host_seed(&self, argv: &[&str]) -> Result<(), SandboxError> {
        // The shim's seed budget for both seed kinds.
        const SEED_BUDGET: Duration = Duration::from_secs(300);
        let root = self.root.map("/")?;
        let output = tokio::process::Command::new("timeout")
            .arg("--kill-after=5")
            .arg(SEED_BUDGET.as_secs().to_string())
            .args(argv)
            .current_dir(root)
            .output()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        let code = output.status.code();
        if code == Some(124) || code == Some(137) {
            return Err(SandboxError::Timeout(SEED_BUDGET.as_secs()));
        }
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
}

/// The container-side path of the sandbox cwd: sandbox paths are
/// mount-relative, so the container path is `workspace` plus the cwd.
/// `None` for the unset cwd (`""`) — `docker exec` keeps its own
/// default working directory.
fn container_cwd(workspace: &str, cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
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

/// The exec argv: `docker exec [--workdir <container cwd>]
/// <container> timeout --kill-after=5 <secs> bash -c <command>`.
/// `docker exec` has its own `--workdir`, preferred over the shim's
/// `cd` prefix — cleaner, and no shell quoting for the directory.
fn exec_argv(
    container: &str,
    workspace: &str,
    timeout_secs: u64,
    cwd: &str,
    command: &str,
) -> Vec<String> {
    let mut argv = vec!["docker".to_owned(), "exec".to_owned()];
    if let Some(dir) = container_cwd(workspace, cwd) {
        argv.push("--workdir".to_owned());
        argv.push(dir);
    }
    argv.extend([
        container.to_owned(),
        "timeout".to_owned(),
        "--kill-after=5".to_owned(),
        timeout_secs.to_string(),
        "bash".to_owned(),
        "-c".to_owned(),
        command.to_owned(),
    ]);
    argv
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        use std::process::Stdio;
        use std::sync::atomic::Ordering;

        // The trait's first observation point: a flip before the start
        // never reaches the container.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        // The cwd is the container's to interpret, not this process's
        // fs: the textual guard is what stands between the model and a
        // `..` climb, the same guard the fs-disjoint backends lean on.
        check_remote_path(&request.cwd)?;
        let argv = exec_argv(
            &self.container,
            &self.workspace,
            request.timeout.as_secs(),
            &request.cwd,
            &request.command,
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
            WorkspaceSeed::GitClone(url) => {
                // Direct argv, no shell: the URL needs no quoting.
                self.run_host_seed(&["git", "clone", url, "."]).await
            }
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                // Staged through the file route, unpacked at the root
                // — the same shape the local backend seeds with, both
                // halves plain fs on the host side of the mount.
                self.write_file(&format!("/{STAGING}"), &bytes).await?;
                let unpacked = self.run_host_seed(&["tar", "-xzf", STAGING]).await;
                // The staging file is not workspace content; a leftover
                // one is harmless, so its removal is best-effort.
                if let Ok(staged) = self.root.map(&format!("/{STAGING}")) {
                    let _ = tokio::fs::remove_file(staged).await;
                }
                unpacked
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        exec_util::read_through(&self.root, path).await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        exec_util::write_through(&self.root, path, contents).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A sandbox over a fresh tempdir standing in for the host side of
    /// the mount; the guard comes back so the root outlives the test.
    fn sandbox() -> (DockerSandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        (
            DockerSandbox::new("rhema-sandbox-test", root, "/workspace"),
            dir,
        )
    }

    #[test]
    fn the_exec_argv_carries_the_workdir_and_the_timeout_prefix() {
        assert_eq!(
            exec_argv("box", "/workspace", 30, "/src", "echo hi"),
            vec![
                "docker",
                "exec",
                "--workdir",
                "/workspace/src",
                "box",
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
            exec_argv("box", "/workspace", 30, "/", "echo hi"),
            vec![
                "docker",
                "exec",
                "--workdir",
                "/workspace",
                "box",
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
    fn an_unset_cwd_leaves_docker_execs_default_directory() {
        let argv = exec_argv("box", "/workspace", 30, "", "echo hi");
        assert_eq!(argv[0], "docker");
        assert_eq!(argv[1], "exec");
        assert!(!argv.contains(&"--workdir".to_owned()));
        assert_eq!(&argv[2..6], &["box", "timeout", "--kill-after=5", "30"]);
    }

    #[test]
    fn the_container_cwd_joins_mount_and_sandbox_path() {
        assert_eq!(
            container_cwd("/workspace", "/src/deep").as_deref(),
            Some("/workspace/src/deep")
        );
        assert_eq!(
            container_cwd("/workspace", "/").as_deref(),
            Some("/workspace")
        );
        assert_eq!(
            container_cwd("/workspace/", "/src").as_deref(),
            Some("/workspace/src")
        );
        assert_eq!(container_cwd("/", "/src").as_deref(), Some("/src"));
        assert_eq!(container_cwd("/", "/").as_deref(), Some("/"));
        assert_eq!(container_cwd("/workspace", ""), None);
    }

    #[tokio::test]
    async fn a_relative_cwd_is_refused_without_spawning_docker() {
        let (sandbox, _dir) = sandbox();
        let error = sandbox
            .exec(
                ExecRequest {
                    command: "pwd".into(),
                    cwd: "workspace".into(),
                    timeout: Duration::from_secs(30),
                },
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .await
            .expect_err("the relative cwd is refused");
        assert!(
            matches!(error, SandboxError::Failed(_)),
            "the refusal is an in-band failure, not a crash: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_dot_dot_cwd_is_refused_without_spawning_docker() {
        let (sandbox, _dir) = sandbox();
        let error = sandbox
            .exec(
                ExecRequest {
                    command: "pwd".into(),
                    cwd: "/../etc".into(),
                    timeout: Duration::from_secs(30),
                },
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .await
            .expect_err("the climb is refused");
        assert!(
            matches!(error, SandboxError::Failed(_)),
            "the refusal is an in-band failure, not a crash: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_flip_before_the_start_never_reaches_the_container() {
        let (sandbox, _dir) = sandbox();
        let cancelled = AtomicBool::new(true);
        let error = sandbox
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
            .expect_err("a cancelled run does not start");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
    }

    #[tokio::test]
    async fn files_round_trip_through_the_mapped_root() {
        let (sandbox, dir) = sandbox();
        sandbox
            .write_file("/workspace/deep/note.md", b"# note")
            .await
            .expect("the write lands");
        assert_eq!(
            std::fs::read(dir.path().join("workspace/deep/note.md"))
                .expect("the host side of the mount holds the file"),
            b"# note"
        );
        let read = sandbox
            .read_file("/workspace/deep/note.md")
            .await
            .expect("the read lands");
        assert_eq!(read, b"# note");
    }

    #[tokio::test]
    async fn a_missing_file_read_fails_in_band() {
        let (sandbox, _dir) = sandbox();
        let error = sandbox
            .read_file("/workspace/absent")
            .await
            .expect_err("the read fails");
        assert!(
            matches!(error, SandboxError::Failed(_)),
            "a missing file is an in-band failure: {error:?}"
        );
    }

    /// Runs one git command in `dir`; the global and system config are
    /// blanked so the fixture does not depend on the host's.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Builds a one-commit repository at a fresh tempdir — the fixture
    /// the git-seed test clones.
    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("seeded.txt"), b"from git")
            .expect("the fixture file is written");
        git(dir.path(), &["add", "."]);
        git(
            dir.path(),
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        dir
    }

    #[tokio::test]
    async fn seeding_from_a_repository_populates_the_mounted_root() {
        if !docker_available() {
            println!("skipping: no docker daemon");
            return;
        }
        let repo = fixture_repo();
        let (sandbox, dir) = sandbox();
        sandbox
            .seed(&WorkspaceSeed::GitClone(
                repo.path().to_string_lossy().into_owned(),
            ))
            .await
            .expect("the clone lands");
        assert!(
            dir.path().join(".git").exists(),
            "the clone is rooted at the host side of the mount"
        );
        assert_eq!(
            std::fs::read(dir.path().join("seeded.txt")).expect("the seeded file is read"),
            b"from git"
        );
    }

    #[tokio::test]
    async fn a_failed_clone_fails_in_band() {
        if !docker_available() {
            println!("skipping: no docker daemon");
            return;
        }
        let (sandbox, _dir) = sandbox();
        let error = sandbox
            .seed(&WorkspaceSeed::GitClone("/nonexistent-rhema-repo".into()))
            .await
            .expect_err("the clone fails");
        assert!(
            matches!(error, SandboxError::Failed(_)),
            "a failed clone is an in-band failure: {error:?}"
        );
    }

    #[tokio::test]
    async fn seeding_from_a_tarball_unpacks_into_the_mounted_root() {
        if !docker_available() {
            println!("skipping: no docker daemon");
            return;
        }
        let payload = tempfile::tempdir().expect("tempdir");
        std::fs::write(payload.path().join("packed.txt"), b"from tar")
            .expect("the payload file is written");
        let archive = payload.path().join("packed.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(payload.path())
            .arg("packed.txt")
            .status()
            .expect("tar runs");
        assert!(status.success(), "the fixture tarball is built");
        let bytes = std::fs::read(&archive).expect("the tarball is read");
        let (sandbox, dir) = sandbox();
        sandbox
            .seed(&WorkspaceSeed::Tarball {
                name: "packed.tar.gz".into(),
                tar_gz_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
            .await
            .expect("the unpack lands");
        assert_eq!(
            std::fs::read(dir.path().join("packed.txt")).expect("the unpacked file is read"),
            b"from tar"
        );
        assert!(
            !dir.path().join(STAGING).exists(),
            "the staging file does not linger"
        );
    }

    /// True when a docker daemon answers — the gate for the exec tests
    /// that need a real container. Standing a container up needs an
    /// image pull, a network dependency a plain `cargo test` must not
    /// take, so the exec tests skip rather than fail without one.
    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// The running container a live exec test may drive:
    /// `RHEMA_TEST_DOCKER_CONTAINER` naming one, or none — the test
    /// skips. The file routes above cover everything the mount gives;
    /// these exercise the `docker exec` wrap alone.
    fn live_container() -> Option<String> {
        if !docker_available() {
            return None;
        }
        std::env::var("RHEMA_TEST_DOCKER_CONTAINER").ok()
    }

    /// One exec through the sandbox with nothing cancelled and nothing
    /// watching the progress.
    async fn run(
        sandbox: &DockerSandbox,
        request: ExecRequest,
    ) -> Result<ExecOutcome, SandboxError> {
        let cancelled = AtomicBool::new(false);
        sandbox.exec(request, &cancelled, &mut |_| {}).await
    }

    #[tokio::test]
    async fn exec_runs_a_command_inside_the_live_container() {
        let Some(container) = live_container() else {
            println!("skipping: no docker daemon, or RHEMA_TEST_DOCKER_CONTAINER unset");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        let sandbox = DockerSandbox::new(container, root, "/workspace");
        let cancelled = AtomicBool::new(false);
        let mut lines: Vec<String> = Vec::new();
        let outcome = sandbox
            .exec(
                ExecRequest {
                    command: "echo hi".into(),
                    cwd: "/".into(),
                    timeout: Duration::from_secs(30),
                },
                &cancelled,
                &mut |line| lines.push(line.to_owned()),
            )
            .await
            .expect("the command runs in the container");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");
        assert_eq!(lines, vec!["hi\n".to_owned()]);
    }

    #[tokio::test]
    async fn a_live_command_past_its_budget_is_a_timeout() {
        let Some(container) = live_container() else {
            println!("skipping: no docker daemon, or RHEMA_TEST_DOCKER_CONTAINER unset");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        let sandbox = DockerSandbox::new(container, root, "/workspace");
        let error = run(
            &sandbox,
            ExecRequest {
                command: "sleep 5".into(),
                cwd: "/".into(),
                timeout: Duration::from_secs(1),
            },
        )
        .await
        .expect_err("the budget kills the command");
        assert_eq!(error, SandboxError::Timeout(1));
    }
}
