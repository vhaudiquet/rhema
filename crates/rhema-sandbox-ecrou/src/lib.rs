//! The ecrou backend: one microVM per command, the workspace as a
//! share.
//!
//! [`EcrouSandbox`] is the fs-sharing family's microVM member (spec
//! §4's "VM / microVM" row, the ecrou shape): the host workspace
//! crosses every boot as a virtiofs share, so the file routes are
//! plain fs through [`FsRoot`] — as on the docker bind mount — and
//! only exec crosses the `ecrou` CLI. There is no instance to stand
//! up or stop: ecrou runs one-shot machines, so every exec boots a
//! fresh VM from the image, runs the command with the workspace
//! shared in, and powers the machine off when the command exits.
//! Package installs and other rootfs state live exactly one command;
//! the workspace is what persists, which is the seam's whole contract.
//!
//! Exec is `ecrou run --quiet --capture <io> --share <ws>:<ws>
//! --workdir <cwd> <image> -- /bin/sh -c <line>`: the workspace share
//! mounts at its own host path (the docker 1:1 posture), the cwd
//! rides `--workdir`, and the line runs the command through `bash -c`
//! when the image carries bash, through its `sh` when it does not.
//! The capture directory is a scratch tempdir ecrou takes as an extra
//! share: the payload's stdout and stderr land there as two files,
//! the console (kernel and boot chatter, merged) is discarded, and
//! the pump tails the two files while the machine runs — complete
//! lines through `progress`, a capped tail each, the same shape
//! `exec_util` gives the pipe-backed backends.
//!
//! The budget and the cancel are the kill, host-side: killing the
//! `ecrou` process kills the machine with it (the VMM child carries
//! `PR_SET_PDEATHSIG`), stronger than the docker prefix, which leaves
//! the command running in the container. The boot rides inside the
//! wall-clock budget — a cold image pays its download once into the
//! ecrou cache, every command pays its extract-plus-boot (under 2 s
//! on a warm alpine, more on a distro rootfs). Anything `ecrou`
//! itself reports (`Error:` on its stderr) is an unavailable sandbox,
//! not a guest result.
#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use rhema_sandbox::exec_util::{self, MAX_STREAM_BYTES, STAGING, under_workspace};
use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The seed budget for both seed kinds, the siblings' number: the
/// seed runs host-side, so the budget is the same `timeout` prefix
/// every host command here gets.
const SEED_BUDGET: Duration = Duration::from_secs(300);

/// How often the capture files are tailed and the cancel flag and the
/// budget sampled. The machine has no stdout to block on — the
/// console rides a nulled pipe — so this tick is the only observation
/// point a silent command's kill has.
const TICK: Duration = Duration::from_millis(100);

/// The ecrou sandbox a session's commands run in: every exec boots
/// one microVM of `image` with the host root shared in at its own
/// path.
#[derive(Debug, Clone)]
pub struct EcrouSandbox {
    /// The image reference every boot resolves: an ecrou distro name
    /// (`ubuntu`, `alpine:3.23`) or an OCI image reference.
    image: String,
    /// The host directory standing in as the sandbox filesystem — the
    /// host side of the workspace share.
    root: FsRoot,
    /// The sandbox-side path of the workspace: the path the prompt
    /// names, stripped from sandbox paths before they map through
    /// [`FsRoot`] or join the guest's mount point.
    workspace: String,
}

impl EcrouSandbox {
    /// A sandbox booting `image` with the host root `root` shared in,
    /// named `workspace` on the sandbox side.
    pub fn new(image: impl Into<String>, root: FsRoot, workspace: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            root,
            workspace: workspace.into(),
        }
    }

    /// The guest path of the sandbox cwd: the workspace share mounts
    /// at its own canonical host path, so the mount point plus the
    /// prefix-stripped tail is where the command lands. The empty and
    /// root cwd land at the mount itself.
    fn guest_cwd(&self, cwd: &str) -> Result<String, SandboxError> {
        // The guard first: the cwd is model-influenced, and the root
        // refuses relatives, climbs, and link escapes.
        self.root.map(cwd)?;
        let tail = under_workspace(&self.workspace, cwd);
        let mount = self.mount_point();
        Ok(match tail.as_str() {
            "" | "/" => mount,
            _ => format!("{mount}{tail}"),
        })
    }

    /// The workspace share's host path, canonical — the path both
    /// sides of the share use, so a workspace reached through a link
    /// still mounts under the name its files resolve to.
    fn mount_point(&self) -> String {
        self.root
            .map("")
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The payload line: the command through `bash -c` when the image
/// carries bash, through its `sh` when it does not — the seam says
/// `bash -c`, and an alpine rootfs ships no bash. `exec` hands the
/// streams and the exit code straight through either way.
fn shell_line(command: &str) -> String {
    let quoted = exec_util::shquote(command);
    format!(
        "if command -v bash >/dev/null 2>&1; then \
         exec bash -c {quoted}; else exec sh -c {quoted}; fi"
    )
}

/// The run argv: options first, the image, then the payload after
/// `--`. Nothing in the payload is quoted for a middle shell — the
/// argv crosses the CLI as argv, and the init script's own quoting
/// takes it from there.
fn run_argv(image: &str, capture: &Path, share: &str, workdir: &str, command: &str) -> Vec<String> {
    vec![
        "ecrou".to_owned(),
        "run".to_owned(),
        // The console is diagnostics; the payload's streams cross the
        // capture share. Quiet also keeps the progress renderer off
        // stderr, which this process reads.
        "--quiet".to_owned(),
        "--capture".to_owned(),
        capture.to_string_lossy().into_owned(),
        "--share".to_owned(),
        share.to_owned(),
        "--workdir".to_owned(),
        workdir.to_owned(),
        image.to_owned(),
        "--".to_owned(),
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        shell_line(command),
    ]
}

/// One capture file, tailed: the read offset, and the capped tail of
/// the complete lines seen so far.
#[derive(Debug, Default)]
struct StreamTail {
    path: std::path::PathBuf,
    pos: u64,
    tail: Vec<u8>,
}

impl StreamTail {
    /// Reads what appeared since the last tick, emits every complete
    /// line through `progress`, and keeps the capped tail. `final_pass`
    /// — the drain after the machine is gone — also consumes a
    /// trailing partial line as a line, the way the pipe pump's
    /// `lines()` yields it at end of stream; mid-run, a partial line
    /// waits for its newline. The file does not exist until the
    /// guest's first write — absence is silence, not an error.
    async fn pump(
        &mut self,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
        final_pass: bool,
    ) -> Result<(), SandboxError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt as _};

        let Ok(mut file) = tokio::fs::File::open(&self.path).await else {
            // The guest creates the file on its first write; a boot
            // that dies early leaves it absent for the whole call.
            return Ok(());
        };
        let len = file
            .metadata()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?
            .len();
        if len <= self.pos {
            return Ok(());
        }
        file.seek(std::io::SeekFrom::Start(self.pos))
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?;
        let mut bytes = Vec::with_capacity((len - self.pos) as usize);
        file.read_to_end(&mut bytes)
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?;
        let complete = match bytes.iter().rposition(|b| *b == b'\n') {
            Some(at) => &bytes[..=at],
            // No complete line: a final pass takes the partial tail as
            // a line, a mid-run tick waits for the newline and reads
            // the whole partial line again next tick.
            None if !final_pass => return Ok(()),
            None => bytes.as_slice(),
        };
        self.pos += complete.len() as u64;
        if complete.last() != Some(&b'\n') {
            let line = String::from_utf8_lossy(complete).into_owned();
            progress(&format!("{line}\n"));
            append_capped(&mut self.tail, format!("{line}\n").as_bytes());
        } else {
            for line in complete.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                progress(&format!("{}\n", String::from_utf8_lossy(line)));
            }
            append_capped(&mut self.tail, complete);
        }
        Ok(())
    }

    /// The capped tail, the pump's.
    fn take(self) -> Vec<u8> {
        self.tail
    }
}

/// Appends `bytes` to `tail`, keeping at most the last
/// [`MAX_STREAM_BYTES`] — the same cap the pipe pump applies as the
/// bytes land.
fn append_capped(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > MAX_STREAM_BYTES {
        let excess = tail.len() - MAX_STREAM_BYTES;
        tail.drain(..excess);
    }
}

/// The stderr lines that are ecrou's own words, not the guest's: the
/// runtime's `Error:` diagnostics and clap's lowercase `error:` usage
/// complaints. The payload's streams never touch this stderr — they
/// ride the capture files — so a warning is the only thing that may
/// appear next to a guest's own nonzero exit.
fn is_ecrou_error(line: &str) -> bool {
    line.starts_with("Error: ") || line.starts_with("error: ")
}

/// Reads the ecrou process's stderr and sorts the exit out: a line of
/// ecrou's own next to a nonzero exit is a failed boot, an image that
/// would not resolve, an old binary — an unavailable sandbox, never a
/// guest result. Warnings ride along on successes and on guest
/// failures alike; a guest exit is the command's own result.
fn classify(
    code: Option<i32>,
    ecrou_stderr: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<ExecOutcome, SandboxError> {
    let failed = code != Some(0) && ecrou_stderr.lines().any(is_ecrou_error);
    if failed {
        let detail = ecrou_stderr
            .lines()
            .rev()
            .find(|line| is_ecrou_error(line))
            .map(|line| {
                line.trim_start_matches("Error: ")
                    .trim_start_matches("error: ")
                    .to_owned()
            })
            .unwrap_or_else(|| ecrou_stderr.trim().to_owned());
        return Err(SandboxError::Unavailable(detail));
    }
    Ok(ExecOutcome {
        stdout,
        stderr,
        code,
    })
}

/// Whether the host answers a live-machine test: `ecrou probe` green,
/// a current binary (the `--capture` flag exists), an alpine tarball
/// already in the cache — the tests run no downloads — and one smoke
/// boot with a shared tempdir, the exact posture the tests use. The
/// answer is cached: the smoke boot is a real machine, and the four
/// live tests want it once.
#[cfg(all(test, unix))]
fn ecrou_ready() -> bool {
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(|| {
        let answers = |args: &[&str]| {
            std::process::Command::new("ecrou")
                .args(args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        };
        if !answers(&["probe"]) {
            return false;
        }
        let help = std::process::Command::new("ecrou")
            .args(["run", "--help"])
            .stdin(std::process::Stdio::null())
            .output();
        let current = help
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("--capture"))
            .unwrap_or(false);
        if !current {
            return false;
        }
        let Ok(cache) = std::env::var("HOME") else {
            return false;
        };
        let cache = Path::new(&cache).join(".cache").join("ecrou");
        let cached = std::fs::read_dir(&cache)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("alpine") && name.ends_with(".tar.gz")
                })
            })
            .unwrap_or(false);
        if !cached {
            return false;
        }
        // The smoke boot: one machine, a tempdir share — the whole
        // path a live test takes, minus the assertions.
        let Ok(smoke) = tempfile::tempdir() else {
            return false;
        };
        let share = smoke.path().to_string_lossy().into_owned();
        std::process::Command::new("ecrou")
            .args([
                "run",
                "--quiet",
                "--capture",
                smoke.path().join("io").to_str().unwrap_or_default(),
                "--share",
                &format!("{share}:{share}"),
                "--workdir",
                &share,
                "alpine:3.23",
                "--",
                "/bin/sh",
                "-c",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

#[async_trait]
impl Sandbox for EcrouSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        use std::process::Stdio;

        // The trait's first observation point: a flip before the start
        // never boots a machine.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let workdir = self.guest_cwd(&request.cwd)?;
        let mount = self.mount_point();
        // The capture directory lives one call: the guest writes the
        // payload's streams into it through the share ecrou takes,
        // and the drop deletes whatever the run left.
        let capture = tempfile::tempdir()
            .map_err(|err| SandboxError::Unavailable(format!("the capture directory: {err}")))?;
        let argv = run_argv(
            &self.image,
            capture.path(),
            &format!("{mount}:{mount}"),
            &workdir,
            &request.command,
        );
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            // The console (merged kernel and boot chatter) is this
            // backend's discarded stream; stderr carries only ecrou's
            // own words, drained after the exit.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // The explicit kill below is the budget and the cancel;
            // the drop is the backstop for the early returns, and a
            // killed ecrou takes the machine down with it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;

        let mut out = StreamTail {
            path: capture.path().join("stdout"),
            ..StreamTail::default()
        };
        let mut err = StreamTail {
            path: capture.path().join("stderr"),
            ..StreamTail::default()
        };
        let started = Instant::now();
        let code = loop {
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {
                    out.pump(progress, false).await?;
                    err.pump(progress, false).await?;
                    if cancelled.load(Ordering::Relaxed) {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(SandboxError::Failed("cancelled".into()));
                    }
                    if started.elapsed() >= request.timeout {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(SandboxError::Timeout(request.timeout.as_secs()));
                    }
                }
                status = child.wait() => {
                    break status.map_err(|err| {
                        SandboxError::Unavailable(format!("waiting for the machine: {err}"))
                    })?;
                }
            }
        };
        // The machine is gone; the last output landed in the capture
        // files right before it died. Drain once more, partial line
        // included.
        out.pump(progress, true).await?;
        err.pump(progress, true).await?;
        // Nothing reads stderr while the machine runs — ecrou writes
        // a warning or two, never a stream — so the drain after the
        // exit cannot have filled the pipe.
        let mut ecrou_stderr = Vec::new();
        if let Some(mut pipe) = child.stderr.take() {
            tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut ecrou_stderr)
                .await
                .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        }
        classify(
            code.code(),
            String::from_utf8_lossy(&ecrou_stderr).into_owned(),
            out.take(),
            err.take(),
        )
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        // The workspace is host fs: the seed needs no machine.
        match seed {
            WorkspaceSeed::GitClone(url) => self.run_host_seed(&["git", "clone", url, "."]).await,
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
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
        exec_util::read_through(&self.root, &under_workspace(&self.workspace, path)).await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        exec_util::write_through(
            &self.root,
            &under_workspace(&self.workspace, path),
            contents,
        )
        .await
    }
}

impl EcrouSandbox {
    /// Runs one seed command on the host, at the root the machines
    /// share: the budget and kill are the same `timeout` prefix, and
    /// a nonzero exit surfaces the stderr tail, the way the shim's
    /// seed route reports a failed clone or unpack.
    async fn run_host_seed(&self, argv: &[&str]) -> Result<(), SandboxError> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sandbox() -> (EcrouSandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
        let ws = dir.path().to_string_lossy().into_owned();
        (EcrouSandbox::new("alpine:3.23", root, ws), dir)
    }

    fn request(command: &str) -> ExecRequest {
        ExecRequest {
            command: command.to_owned(),
            cwd: "/".into(),
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(
        sandbox: &EcrouSandbox,
        request: ExecRequest,
    ) -> Result<ExecOutcome, SandboxError> {
        sandbox
            .exec(request, &AtomicBool::new(false), &mut |_| {})
            .await
    }

    #[test]
    fn the_payload_line_prefers_bash_and_falls_back_to_sh() {
        let line = shell_line("echo hi");
        assert!(line.starts_with("if command -v bash >/dev/null 2>&1; then"));
        assert!(line.contains("exec bash -c 'echo hi'"));
        assert!(line.ends_with("else exec sh -c 'echo hi'; fi"));
    }

    #[test]
    fn the_command_is_quoted_inside_the_payload_line() {
        let line = shell_line("echo it's; rm -rf $HOME");
        assert!(
            line.contains(r"'echo it'\''s; rm -rf $HOME'"),
            "the command rides as one quoted operand: {line}"
        );
    }

    #[test]
    fn the_run_argv_names_capture_share_workdir_then_the_payload() {
        let argv = run_argv(
            "ubuntu",
            Path::new("/tmp/io"),
            "/home/me/proj:/home/me/proj",
            "/home/me/proj/src",
            "echo hi",
        );
        let expected = [
            "ecrou",
            "run",
            "--quiet",
            "--capture",
            "/tmp/io",
            "--share",
            "/home/me/proj:/home/me/proj",
            "--workdir",
            "/home/me/proj/src",
            "ubuntu",
            "--",
            "/bin/sh",
            "-c",
        ] as [&str; 13];
        assert_eq!(&argv[..expected.len()], expected);
        assert_eq!(argv.len(), expected.len() + 1);
        assert!(argv[argv.len() - 1].starts_with("if command -v bash"));
    }

    #[test]
    fn append_capped_keeps_only_the_last_cap() {
        let mut tail = Vec::new();
        append_capped(&mut tail, &[b'x'; MAX_STREAM_BYTES * 3]);
        assert_eq!(tail.len(), MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn the_tail_pump_reads_complete_lines_and_holds_the_partial() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out");
        std::fs::write(&path, b"first\nsec").expect("the first chunk is written");
        let mut stream = StreamTail {
            path,
            ..StreamTail::default()
        };
        let mut lines: Vec<String> = Vec::new();
        stream
            .pump(&mut |line| lines.push(line.to_owned()), false)
            .await
            .expect("the first pump reads");
        assert_eq!(lines, vec!["first\n".to_owned()]);
        // The partial line is still on disk, unread; the next chunk
        // completes it.
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&stream.path)
            .expect("the file reopens");
        file.write_all(b"ond\n").expect("the second chunk lands");
        drop(file);
        stream
            .pump(&mut |line| lines.push(line.to_owned()), false)
            .await
            .expect("the second pump reads");
        assert_eq!(lines, vec!["first\n".to_owned(), "second\n".to_owned()]);
        assert_eq!(stream.take(), b"first\nsecond\n".to_vec());
    }

    #[tokio::test]
    async fn the_final_pass_takes_a_trailing_partial_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out");
        std::fs::write(&path, b"no newline at the end").expect("the chunk is written");
        let mut stream = StreamTail {
            path,
            ..StreamTail::default()
        };
        let mut lines: Vec<String> = Vec::new();
        stream
            .pump(&mut |line| lines.push(line.to_owned()), true)
            .await
            .expect("the final pump reads");
        assert_eq!(lines, vec!["no newline at the end\n".to_owned()]);
        assert_eq!(stream.take(), b"no newline at the end\n".to_vec());
    }

    #[tokio::test]
    async fn the_tail_pump_treats_an_absent_file_as_silence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut stream = StreamTail {
            path: dir.path().join("never-written"),
            ..StreamTail::default()
        };
        let mut lines: Vec<String> = Vec::new();
        stream
            .pump(&mut |line| lines.push(line.to_owned()), false)
            .await
            .expect("absence is silence");
        assert!(lines.is_empty());
    }

    #[test]
    fn an_ecrou_error_reads_as_an_unavailable_sandbox() {
        let error = classify(
            Some(1),
            "Warning: could not check image freshness\nError: the machine failed to boot\n"
                .to_owned(),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("the boot failure is an unavailable sandbox");
        assert_eq!(
            error,
            SandboxError::Unavailable("the machine failed to boot".to_owned())
        );
    }

    #[test]
    fn a_stale_binary_reads_as_an_unavailable_sandbox() {
        let error = classify(
            Some(2),
            "error: unexpected argument '--capture' found\n\nUsage: ecrou run --quiet <IMAGE>\n"
                .to_owned(),
            Vec::new(),
            Vec::new(),
        )
        .expect_err("the usage error is an unavailable sandbox");
        assert_eq!(
            error,
            SandboxError::Unavailable("unexpected argument '--capture' found".to_owned())
        );
    }

    #[test]
    fn a_guest_exit_rides_with_warnings_ignored() {
        let outcome = classify(
            Some(3),
            "Warning: could not check image freshness (dns); using cache\n".to_owned(),
            b"out".to_vec(),
            b"err".to_vec(),
        )
        .expect("the guest exit is the command's own result");
        assert_eq!(outcome.code, Some(3));
        assert_eq!(outcome.stdout, b"out");
        assert_eq!(outcome.stderr, b"err");
    }

    #[test]
    fn a_zero_exit_rides_whatever_stderr_says() {
        let outcome = classify(Some(0), String::new(), b"ok\n".to_vec(), Vec::new())
            .expect("a zero exit is a done command");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"ok\n");
    }

    #[tokio::test]
    async fn a_flip_before_the_start_never_boots_a_machine() {
        let (sandbox, _dir) = sandbox();
        let cancelled = AtomicBool::new(true);
        let error = sandbox
            .exec(request("echo hi"), &cancelled, &mut |_| {})
            .await
            .expect_err("a cancelled run does not boot");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
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

    #[test]
    fn the_guest_cwd_joins_the_mount_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
        let mount = std::fs::canonicalize(dir.path())
            .expect("the tempdir resolves")
            .to_string_lossy()
            .into_owned();
        let sandbox = EcrouSandbox::new("alpine:3.23", root, mount.clone());
        assert_eq!(sandbox.guest_cwd("").expect("the empty cwd maps"), mount);
        assert_eq!(sandbox.guest_cwd("/").expect("the root maps"), mount);
        assert_eq!(
            sandbox
                .guest_cwd(&format!("{mount}/src/deep"))
                .expect("the nested cwd maps"),
            format!("{mount}/src/deep")
        );
    }

    // The live-machine tests below boot one alpine each; they skip
    // where ecrou cannot run or the image is not cached, and the
    // readiness check keeps the suite honest about which one it was.

    #[tokio::test]
    async fn a_command_output_and_its_exit_code_cross_the_capture() {
        if !ecrou_ready() {
            println!("skipping: no runnable ecrou or no cached alpine image");
            return;
        }
        let (sandbox, _dir) = sandbox();
        let mut lines: Vec<String> = Vec::new();
        let outcome = {
            let cancelled = AtomicBool::new(false);
            sandbox
                .exec(
                    request("echo from the vm; echo to err 1>&2; exit 7"),
                    &cancelled,
                    &mut |line| lines.push(line.to_owned()),
                )
                .await
                .expect("the machine runs")
        };
        assert_eq!(outcome.code, Some(7));
        assert_eq!(outcome.stdout, b"from the vm\n");
        assert_eq!(outcome.stderr, b"to err\n");
        assert!(
            lines.contains(&"from the vm\n".to_owned()),
            "the line streamed through progress: {lines:?}"
        );
    }

    #[tokio::test]
    async fn the_shared_workspace_is_the_same_tree_both_sides() {
        if !ecrou_ready() {
            println!("skipping: no runnable ecrou or no cached alpine image");
            return;
        }
        let (sandbox, dir) = sandbox();
        // The host write and the guest read meet through the share:
        // the sandbox path /note.txt is <root>/note.txt host-side and
        // <mount>/note.txt guest-side, reached over the workspace cwd.
        sandbox
            .write_file("/note.txt", b"from the host\n")
            .await
            .expect("the write lands");
        let ws = dir.path().to_string_lossy().into_owned();
        let outcome = run(
            &sandbox,
            ExecRequest {
                command: "cat note.txt && echo guest >> note.txt".into(),
                cwd: ws,
                timeout: Duration::from_secs(30),
            },
        )
        .await
        .expect("the machine runs");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"from the host\n");
        assert_eq!(
            std::fs::read(dir.path().join("note.txt")).expect("the host side reads"),
            b"from the host\nguest\n"
        );
    }

    #[tokio::test]
    async fn a_command_past_its_budget_is_a_timeout_and_the_machine_dies() {
        if !ecrou_ready() {
            println!("skipping: no runnable ecrou or no cached alpine image");
            return;
        }
        let (sandbox, _dir) = sandbox();
        let started = Instant::now();
        let error = run(
            &sandbox,
            ExecRequest {
                command: "sleep 60".into(),
                cwd: "/".into(),
                timeout: Duration::from_secs(2),
            },
        )
        .await
        .expect_err("the budget kills the command");
        assert_eq!(error, SandboxError::Timeout(2));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the kill lands near the budget, the boot inside it: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_flip_mid_run_kills_the_machine() {
        if !ecrou_ready() {
            println!("skipping: no runnable ecrou or no cached alpine image");
            return;
        }
        let (sandbox, _dir) = sandbox();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error = sandbox
            .exec(request("sleep 60"), &cancelled, &mut |_| {})
            .await
            .expect_err("the flip stops the run");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the machine dies on the flip, not at its own pace: {:?}",
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

    /// Builds a one-commit repository at a fresh tempdir — the fixture
    /// the git-seed test clones. Skips the test when git or the
    /// GNU `timeout` the seed budget leans on are missing.
    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
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
        std::fs::write(dir.path().join("seeded.txt"), b"from git")
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
        dir
    }
}
