//! The local backend: processes rooted at a host directory.
//!
//! [`LocalSandbox`] is the fs-sharing family's direct member (spec §4's
//! "local direct" row): exec runs the shim's argv —
//! `timeout --kill-after=5 <secs> bash -c <command>` — as a child of
//! this process, its working directory mapped through the
//! [`FsRoot`](rhema_sandbox::FsRoot). The kill is the prefix's, not
//! tokio's: the budget is the mechanism the spec's kill-semantics
//! decision (§4/§9) fixed, so `ExecRequest::timeout` means the same
//! thing on every backend that lacks a per-command kill — including
//! this one. `timeout` kills at the budget (exit 124) and
//! `--kill-after` escalates to SIGKILL, which the shell reports as 137;
//! those two codes are how a run is read as a timeout, the shim's
//! `timed_out` detection restated for a process child. Streams pump
//! live lines and keep a capped tail each, the shim's caps restated
//! host-side — the wiring is `exec_util`'s, shared with the docker
//! and ssh backends.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

use rhema_sandbox::exec_util::{self, STAGING};
use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The local sandbox a session's commands run in: every POSIX path it
/// is handed maps into one host directory.
#[derive(Debug, Clone)]
pub struct LocalSandbox {
    /// The host directory standing in as the sandbox filesystem.
    root: FsRoot,
}

impl LocalSandbox {
    /// A sandbox over `root`.
    pub fn new(root: FsRoot) -> Self {
        Self { root }
    }

    /// Runs one seed command at the root through the ordinary exec
    /// path — the same caps, the same budget, the same kill semantics
    /// as every other command. A nonzero exit surfaces the stderr, the
    /// way the shim's seed route reports a failed clone or unpack.
    async fn run_seed(&self, command: String) -> Result<(), SandboxError> {
        // The shim's seed budget for both seed kinds.
        const SEED_BUDGET: Duration = Duration::from_secs(300);
        let outcome = self
            .exec(
                ExecRequest {
                    command,
                    cwd: "/".into(),
                    timeout: SEED_BUDGET,
                },
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .await?;
        if outcome.code != Some(0) {
            let detail = String::from_utf8_lossy(&outcome.stderr);
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

#[async_trait]
impl Sandbox for LocalSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        use std::process::Stdio;
        use std::sync::atomic::Ordering;

        // The trait's first observation point: a flip before the start
        // never reaches the host.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let cwd = self.root.map(&request.cwd)?;
        let child = tokio::process::Command::new("timeout")
            .arg("--kill-after=5")
            .arg(request.timeout.as_secs().to_string())
            .arg("bash")
            .arg("-c")
            .arg(&request.command)
            .current_dir(cwd)
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
                // The shim's command, quoting included: the URL is
                // config input and rides through a shell.
                self.run_seed(format!("git clone {} .", exec_util::shquote(url)))
                    .await
            }
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                // Staged through the file route, unpacked through the
                // exec route — the same caps and budget every other
                // call gets.
                self.write_file(&format!("/{STAGING}"), &bytes).await?;
                let unpacked = self.run_seed(format!("tar -xzf {STAGING}")).await;
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
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    // The cap the tail test asserts against.
    use rhema_sandbox::exec_util::MAX_STREAM_BYTES;

    /// A sandbox over a fresh tempdir; the guard comes back so the
    /// root outlives the test's calls and vanishes after.
    fn sandbox() -> (LocalSandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        (LocalSandbox::new(root), dir)
    }

    /// One exec request at the root, budgeted for a test.
    fn request(command: &str) -> ExecRequest {
        ExecRequest {
            command: command.to_owned(),
            cwd: "/".into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// One exec through the sandbox with nothing cancelled and nothing
    /// watching the progress — the shape most tests want.
    async fn run(
        sandbox: &LocalSandbox,
        request: ExecRequest,
    ) -> Result<ExecOutcome, SandboxError> {
        let cancelled = AtomicBool::new(false);
        sandbox.exec(request, &cancelled, &mut |_| {}).await
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
    /// the git-seed tests clone.
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
    async fn exec_captures_stdout_and_streams_the_lines() {
        let (sandbox, _dir) = sandbox();
        let cancelled = AtomicBool::new(false);
        let mut lines: Vec<String> = Vec::new();
        let outcome = sandbox
            .exec(request("echo hi"), &cancelled, &mut |line| {
                lines.push(line.to_owned())
            })
            .await
            .expect("the command runs");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");
        assert_eq!(outcome.stderr, b"");
        assert_eq!(lines, vec!["hi\n".to_owned()]);
    }

    #[tokio::test]
    async fn exec_runs_in_the_mapped_working_directory() {
        let (sandbox, _dir) = sandbox();
        sandbox
            .write_file("/workspace/note.txt", b"here")
            .await
            .expect("the write lands");
        let outcome = run(
            &sandbox,
            ExecRequest {
                command: "ls".into(),
                cwd: "/workspace".into(),
                timeout: Duration::from_secs(30),
            },
        )
        .await
        .expect("the command runs");
        assert_eq!(outcome.code, Some(0));
        let listing = String::from_utf8(outcome.stdout).expect("stdout is text");
        assert!(
            listing.contains("note.txt"),
            "the workspace listing shows the file: {listing}"
        );
    }

    #[tokio::test]
    async fn a_nonzero_exit_carries_its_code() {
        let (sandbox, _dir) = sandbox();
        let outcome = run(&sandbox, request("exit 3"))
            .await
            .expect("the command runs");
        assert_eq!(outcome.code, Some(3));
    }

    #[tokio::test]
    async fn a_command_past_its_budget_is_a_timeout() {
        let (sandbox, _dir) = sandbox();
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

    #[tokio::test]
    async fn the_stream_tails_are_capped() {
        let (sandbox, _dir) = sandbox();
        // `seq` writes far past the cap; the outcome keeps only the
        // tail, the way the shim caps before the wire.
        let outcome = run(&sandbox, request("seq 1 20000"))
            .await
            .expect("the command runs");
        assert_eq!(outcome.stdout.len(), MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn files_round_trip_through_the_mapped_root() {
        let (sandbox, _dir) = sandbox();
        sandbox
            .write_file("/workspace/deep/note.md", b"# note")
            .await
            .expect("the write lands");
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

    #[tokio::test]
    async fn a_dot_dot_working_directory_is_refused() {
        let (sandbox, _dir) = sandbox();
        let error = run(
            &sandbox,
            ExecRequest {
                command: "pwd".into(),
                cwd: "/../etc".into(),
                timeout: Duration::from_secs(30),
            },
        )
        .await
        .expect_err("the climb is refused");
        assert!(
            matches!(error, SandboxError::Failed(_)),
            "the refusal is an in-band failure, not a crash: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_flip_before_the_start_never_runs_the_command() {
        let (sandbox, _dir) = sandbox();
        let cancelled = AtomicBool::new(true);
        let error = sandbox
            .exec(request("echo hi"), &cancelled, &mut |_| {})
            .await
            .expect_err("a cancelled run does not start");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
    }

    #[tokio::test]
    async fn a_flip_mid_run_kills_the_command() {
        let (sandbox, _dir) = sandbox();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let error = sandbox
            .exec(request("sleep 30"), &cancelled, &mut |_| {})
            .await
            .expect_err("the flip stops the run");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the command is killed, not left to finish: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn seeding_from_a_local_repository_populates_the_root() {
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
            "the clone is rooted at the sandbox root"
        );
        assert_eq!(
            std::fs::read(dir.path().join("seeded.txt")).expect("the seeded file is read"),
            b"from git"
        );
    }

    #[tokio::test]
    async fn a_failed_clone_fails_in_band() {
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
    async fn seeding_from_a_tarball_unpacks_into_the_root() {
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
}
