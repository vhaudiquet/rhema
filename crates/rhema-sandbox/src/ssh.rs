//! The SSH backend: commands on a remote box, no daemon on the far
//! end.
//!
//! [`SshSandbox`] is the fs-disjoint family's daemonless member (spec
//! §4's SSH row, first variant): zero setup beyond an ssh login. Exec
//! is `ssh <host> -- timeout --kill-after=5 <secs> bash -c <command>`
//! — the same prefix, so the same budget knob and the same 124/137
//! timeout reading, with the streams pumped live through the shared
//! `run_child` wiring (`exec_util`). Files cross as `ssh cat`
//! and `ssh tee`; seeding is a remote `git clone` or a tarball
//! streamed raw to a remote `tar -xzf -`. Every fs operation crosses
//! the boundary, which is what makes the family different — there is
//! no local root to resolve against, so the model-influenced paths are
//! guarded textually by `check_remote_path` (`exec_util`).
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

use crate::exec_util::{check_remote_path, run_child, shquote};
use crate::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The seed budget, the shim's — the same one the fs-sharing backends
/// give their seed commands. A clone or unpack past it is a timeout:
/// the local `ssh` is killed, the remote command may finish.
const SEED_BUDGET: Duration = Duration::from_secs(300);

/// The SSH sandbox a session's commands run in: one remote login, and
/// nothing installed on the far end — that is the mode's reason to
/// exist.
///
/// `host` is an ssh destination passed to `ssh` as-is; any alias or
/// `user@host` the local config resolves works. The remote workspace
/// root — where seeds land — defaults to `/root`; name another
/// directory with [`SshSandbox::with_workspace`]. A git clone seeds
/// into that directory, so as with a fresh cloud workspace it must be
/// empty or absent when the seed lands.
///
/// Cancelling mid-run kills the local `ssh` process; the remote
/// command may finish on the far end — ssh carries no cancel, and
/// this type does not pretend otherwise.
#[derive(Debug, Clone)]
pub struct SshSandbox {
    /// The ssh destination, used verbatim on every command line.
    host: String,
    /// The remote directory standing in as the workspace root.
    workspace: String,
}

impl SshSandbox {
    /// A sandbox on `host`, seeded into the remote `/root`.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            workspace: "/root".to_owned(),
        }
    }

    /// Names the remote directory the workspace root is, where the
    /// seeds land — instead of the `/root` default.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    /// Spawns one local `ssh` carrying `argv` and answers its raw
    /// stdout bytes, its exit code, and its stderr text — the bytes
    /// cross as bytes, no line splitting, which is where the read
    /// route's binary-safety rides. `ssh` absent on this machine is an
    /// unavailable sandbox.
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
            // The seed budget's timeout drops this future past its
            // deadline; the drop is what kills the local ssh.
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

    /// Spawns one local `ssh` carrying `argv` with `bytes` on its
    /// stdin (the pipe closed after), answering the exit code and the
    /// stderr text. The far end's stdout is of no route's use — `tee`
    /// echoes, `tar -xzf -` is quiet — so it goes to null, and a large
    /// feed cannot deadlock on a full stdout pipe.
    async fn feed(
        &self,
        argv: &[String],
        bytes: Vec<u8>,
    ) -> Result<(Option<i32>, String), SandboxError> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Same as `capture`: the seed budget's timeout drops this
            // future, and the drop kills the local ssh.
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(&bytes)
                .await
                .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
            pipe.shutdown()
                .await
                .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("waiting for ssh: {err}")))?;
        Ok((
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// Runs one remote seed under [`SEED_BUDGET`]: past it the local
    /// `ssh` is killed and the budget reads as a timeout; a nonzero
    /// remote exit surfaces the stderr tail.
    async fn run_seed(&self, argv: Vec<String>) -> Result<(), SandboxError> {
        match tokio::time::timeout(SEED_BUDGET, self.capture(&argv)).await {
            Err(_) => Err(SandboxError::Timeout(SEED_BUDGET.as_secs())),
            Ok(answer) => {
                let (_, code, stderr) = answer?;
                match code {
                    Some(0) => Ok(()),
                    code => Err(ssh_failure(code, &stderr)),
                }
            }
        }
    }
}

/// Maps a finished `ssh` onto a sandbox result. Exit 255 is ssh's own
/// "could not reach or speak to the far end" — an unavailable sandbox,
/// not a failed command; any other nonzero exit failed in the remote
/// command, with the stderr tail as the detail.
fn ssh_failure(code: Option<i32>, stderr: &str) -> SandboxError {
    let detail = stderr.trim();
    if code == Some(255) {
        return SandboxError::Unavailable(if detail.is_empty() {
            "ssh could not reach the far end".to_owned()
        } else {
            detail.to_owned()
        });
    }
    SandboxError::Failed(if detail.is_empty() {
        "the remote command failed".to_owned()
    } else {
        detail.to_owned()
    })
}

/// The remote `cd` prefix for the sandbox cwd, quoted for the remote
/// shell — the shim's trick, since ssh has no `--workdir` to prefer.
/// `None` for the empty cwd and the root, which need no `cd`.
fn cd_prefix(cwd: &str) -> Option<String> {
    if cwd.is_empty() || cwd == "/" {
        return None;
    }
    Some(format!("cd {} && ", shquote(cwd)))
}

/// The exec argv: `ssh <host> -- timeout --kill-after=5 <secs>
/// bash -c <command>`, the remote cwd handled by the `cd` prefix. The
/// empty cwd runs in the remote login's default directory.
fn exec_argv(host: &str, timeout_secs: u64, cwd: &str, command: &str) -> Vec<String> {
    let script = match cd_prefix(cwd) {
        Some(prefix) => format!("{prefix}{command}"),
        None => command.to_owned(),
    };
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "timeout".to_owned(),
        "--kill-after=5".to_owned(),
        timeout_secs.to_string(),
        "bash".to_owned(),
        "-c".to_owned(),
        script,
    ]
}

/// The read argv: `ssh <host> -- cat <path>`, the path quoted for the
/// remote shell — ssh joins its command words and the far end
/// reparses them.
fn read_argv(host: &str, path: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "cat".to_owned(),
        shquote(path),
    ]
}

/// The parent directory of an absolute path, `None` at the root —
/// there is no directory above it to make.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => Some(parent.to_owned()),
        _ => None,
    }
}

/// The mkdir argv for the write route's parent step:
/// `ssh <host> -- mkdir -p <parent>`.
fn mkdir_argv(host: &str, parent: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "mkdir".to_owned(),
        "-p".to_owned(),
        shquote(parent),
    ]
}

/// The write argv: `ssh <host> -- tee <path>`, the bytes arriving on
/// stdin.
fn tee_argv(host: &str, path: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "tee".to_owned(),
        shquote(path),
    ]
}

/// The git-seed argv: `ssh <host> -- git clone <url> <workspace>` —
/// the repository becomes the remote workspace root. Both operands
/// are quoted for the remote shell.
fn clone_argv(host: &str, url: &str, workspace: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "git".to_owned(),
        "clone".to_owned(),
        shquote(url),
        shquote(workspace),
    ]
}

/// The tarball-seed argv: `ssh <host> -- tar -xzf - -C <workspace>`,
/// the tarball streamed raw on stdin — no base64 hop, binary-safe.
fn tar_argv(host: &str, workspace: &str) -> Vec<String> {
    vec![
        "ssh".to_owned(),
        host.to_owned(),
        "--".to_owned(),
        "tar".to_owned(),
        "-xzf".to_owned(),
        "-".to_owned(),
        "-C".to_owned(),
        shquote(workspace),
    ]
}

#[async_trait]
impl Sandbox for SshSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        use std::process::Stdio;
        use std::sync::atomic::Ordering;

        // The trait's first observation point: a flip before the start
        // never reaches the wire.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        // There is no local root to resolve against: the textual guard
        // is the whole defense on a remote path.
        check_remote_path(&request.cwd)?;
        // Run from this process's own directory — there is no local
        // root; the sandbox cwd is remote, and rides in the script.
        let argv = exec_argv(
            &self.host,
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
        run_child(child, &request, cancelled, progress).await
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        match seed {
            WorkspaceSeed::GitClone(url) => {
                self.run_seed(clone_argv(&self.host, url, &self.workspace))
                    .await
            }
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                match tokio::time::timeout(
                    SEED_BUDGET,
                    self.feed(&tar_argv(&self.host, &self.workspace), bytes),
                )
                .await
                {
                    Err(_) => Err(SandboxError::Timeout(SEED_BUDGET.as_secs())),
                    Ok(answer) => {
                        let (code, stderr) = answer?;
                        match code {
                            Some(0) => Ok(()),
                            code => Err(ssh_failure(code, &stderr)),
                        }
                    }
                }
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        check_remote_path(path)?;
        let (stdout, code, stderr) = self.capture(&read_argv(&self.host, path)).await?;
        match code {
            Some(0) => Ok(stdout),
            code => Err(ssh_failure(code, &stderr)),
        }
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        check_remote_path(path)?;
        // The shim's put creates the parent directories, and the
        // skills seeding writes two levels deep — one extra ssh call
        // when there is a parent above the root to make.
        if let Some(parent) = parent_of(path) {
            let (_, code, stderr) = self.capture(&mkdir_argv(&self.host, &parent)).await?;
            if code != Some(0) {
                return Err(ssh_failure(code, &stderr));
            }
        }
        let (code, stderr) = self
            .feed(&tee_argv(&self.host, path), contents.to_vec())
            .await?;
        if code != Some(0) {
            return Err(ssh_failure(code, &stderr));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A sandbox aimed at a host no test ever reaches — the pure-argv
    /// and guard tests spawn nothing, so the name only has to be
    /// present.
    fn sandbox() -> SshSandbox {
        SshSandbox::new("rhema-test-invalid").with_workspace("/srv/workspace")
    }

    #[test]
    fn the_exec_argv_carries_the_timeout_prefix_and_the_cd() {
        assert_eq!(
            exec_argv("box", 30, "/src", "echo hi"),
            vec![
                "ssh",
                "box",
                "--",
                "timeout",
                "--kill-after=5",
                "30",
                "bash",
                "-c",
                "cd '/src' && echo hi",
            ]
        );
    }

    #[test]
    fn the_root_and_unset_cwd_need_no_cd() {
        for cwd in ["", "/"] {
            assert_eq!(
                exec_argv("box", 30, cwd, "echo hi"),
                vec![
                    "ssh",
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
    }

    #[test]
    fn the_read_and_write_argvs_quote_their_paths() {
        assert_eq!(
            read_argv("box", "/src/deep/note.md"),
            vec!["ssh", "box", "--", "cat", "'/src/deep/note.md'"]
        );
        assert_eq!(
            tee_argv("box", "/src/deep/note.md"),
            vec!["ssh", "box", "--", "tee", "'/src/deep/note.md'"]
        );
        assert_eq!(
            mkdir_argv("box", "/src/deep"),
            vec!["ssh", "box", "--", "mkdir", "-p", "'/src/deep'"]
        );
    }

    #[test]
    fn the_seed_argvs_quote_url_and_destination() {
        assert_eq!(
            clone_argv("box", "https://example.com/repo", "/srv/workspace"),
            vec![
                "ssh",
                "box",
                "--",
                "git",
                "clone",
                "'https://example.com/repo'",
                "'/srv/workspace'",
            ]
        );
        assert_eq!(
            tar_argv("box", "/srv/workspace"),
            vec![
                "ssh",
                "box",
                "--",
                "tar",
                "-xzf",
                "-",
                "-C",
                "'/srv/workspace'"
            ]
        );
    }

    #[test]
    fn a_cwd_with_spaces_is_quoted_for_the_remote_shell() {
        let argv = exec_argv("box", 30, "/my dir", "echo hi");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("cd '/my dir' && echo hi")
        );
    }

    #[test]
    fn the_parent_of_a_path_stops_at_the_root() {
        assert_eq!(parent_of("/src/deep/note.md").as_deref(), Some("/src/deep"));
        assert_eq!(parent_of("/note.md"), None);
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("/deep/"), None);
    }

    #[test]
    fn ssh_exit_255_is_an_unavailable_sandbox() {
        let error = ssh_failure(Some(255), "");
        assert!(matches!(error, SandboxError::Unavailable(_)), "{error:?}");
        let error = ssh_failure(Some(255), "ssh: connect to host box port 22: refused");
        assert!(
            matches!(error, SandboxError::Unavailable(ref msg) if msg.contains("refused")),
            "{error:?}"
        );
    }

    #[test]
    fn other_remote_exits_are_in_band_failures() {
        let error = ssh_failure(Some(1), "cat: '/absent': No such file or directory");
        assert!(
            matches!(error, SandboxError::Failed(ref msg) if msg.contains("No such file")),
            "{error:?}"
        );
        assert!(matches!(ssh_failure(None, ""), SandboxError::Failed(_)));
    }

    #[tokio::test]
    async fn a_relative_cwd_is_refused_without_spawning_ssh() {
        let error = sandbox()
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
    async fn a_dot_dot_path_is_refused_in_every_route() {
        let sandbox = sandbox();
        for error in [
            sandbox
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
                .expect_err("the cwd climb is refused"),
            sandbox
                .read_file("/../etc/passwd")
                .await
                .expect_err("the read climb is refused"),
            sandbox
                .write_file("/workspace/../escape", b"x")
                .await
                .expect_err("the write climb is refused"),
        ] {
            assert!(
                matches!(error, SandboxError::Failed(_)),
                "the refusal is an in-band failure, not a crash: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_flip_before_the_start_never_reaches_the_wire() {
        let cancelled = AtomicBool::new(true);
        let error = sandbox()
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

    /// True when a real sshd answers on localhost in batch mode — the
    /// gate for the live tests, and a stronger probe than `ssh -V`,
    /// which only proves the binary exists. A plain machine without a
    /// reachable sshd skips rather than fails; no CI default
    /// fabricates the network dependency.
    fn localhost_sshd() -> bool {
        std::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "ConnectTimeout=3",
                "localhost",
                "--",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// One exec through the sandbox with nothing cancelled and nothing
    /// watching the progress.
    async fn run(sandbox: &SshSandbox, request: ExecRequest) -> Result<ExecOutcome, SandboxError> {
        let cancelled = AtomicBool::new(false);
        sandbox.exec(request, &cancelled, &mut |_| {}).await
    }

    #[tokio::test]
    async fn exec_runs_a_command_on_the_localhost_remote() {
        if !localhost_sshd() {
            println!("skipping: no sshd answers on localhost in batch mode");
            return;
        }
        let sandbox = SshSandbox::new("localhost");
        let cancelled = AtomicBool::new(false);
        let mut lines: Vec<String> = Vec::new();
        let outcome = sandbox
            .exec(
                ExecRequest {
                    command: "echo hi".into(),
                    cwd: "/tmp".into(),
                    timeout: Duration::from_secs(30),
                },
                &cancelled,
                &mut |line| lines.push(line.to_owned()),
            )
            .await
            .expect("the command runs remotely");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");
        assert_eq!(lines, vec!["hi\n".to_owned()]);
    }

    #[tokio::test]
    async fn a_live_command_past_its_budget_is_a_timeout() {
        if !localhost_sshd() {
            println!("skipping: no sshd answers on localhost in batch mode");
            return;
        }
        let sandbox = SshSandbox::new("localhost");
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
    async fn files_round_trip_through_the_localhost_remote() {
        if !localhost_sshd() {
            println!("skipping: no sshd answers on localhost in batch mode");
            return;
        }
        let remote = format!("/tmp/rhema-ssh-test-{}", std::process::id());
        let sandbox = SshSandbox::new("localhost").with_workspace(remote.clone());
        sandbox
            .write_file("/deep/note.md", b"# note")
            .await
            .expect("the write lands remotely");
        let read = sandbox
            .read_file("/deep/note.md")
            .await
            .expect("the read lands remotely");
        assert_eq!(read, b"# note");
        // The remote scratch directory is this test's own; its removal
        // is best-effort cleanup, not an assertion.
        let _ = run(
            &sandbox,
            ExecRequest {
                command: format!("rm -rf {}", shquote(&remote)),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn seeding_clones_into_the_remote_workspace_root() {
        if !localhost_sshd() {
            println!("skipping: no sshd answers on localhost in batch mode");
            return;
        }
        // A local-path repository: the remote is localhost, so the
        // fixture's path is as valid remotely as it is here.
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr),
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("seeded.txt"), b"from git")
            .expect("the fixture file is written");
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        let remote = format!("/tmp/rhema-ssh-seed-{}", std::process::id());
        let sandbox = SshSandbox::new("localhost").with_workspace(remote.clone());
        sandbox
            .seed(&WorkspaceSeed::GitClone(
                repo.path().to_string_lossy().into_owned(),
            ))
            .await
            .expect("the remote clone lands");
        let read = sandbox
            .read_file("/seeded.txt")
            .await
            .expect("the cloned file reads back");
        assert_eq!(read, b"from git");
        let _ = run(
            &sandbox,
            ExecRequest {
                command: format!("rm -rf {}", shquote(&remote)),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),
            },
        )
        .await;
    }
}
