//! The execution seam behind the `bash` tool: where commands actually run.
//!
//! [`crate::shell::ShellTools`] owns the tool's contract — the spec, the
//! approval gate, the output caps, the timeout, the error shaping — and
//! delegates the run itself to a [`ShellExecutor`]. The local executor
//! spawns `bash -c` child processes (feature `local-shell`, default on);
//! hosts whose shell lives elsewhere bring their own executor — the cloud
//! deployment runs every command in a remote sandbox through this same
//! seam. The trait's futures are `Send` like every seam in this tree, so
//! the executor object crosses await points freely.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "local-shell")]
use tokio::io::{AsyncBufReadExt, BufReader};

/// What one command left behind: the captured streams and the exit code.
/// The streams carry whatever the executor captured; the tool — not the
/// executor — applies the display caps to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    /// Everything the command wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to stderr.
    pub stderr: Vec<u8>,
    /// The exit code; `None` when the process was terminated by a signal.
    pub code: Option<i32>,
}

/// Why a command did not reach an exit code. In-band: the tool turns
/// every variant into an `is_error` result for the model, never an abort.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShellError {
    /// The command ran past its timeout and was killed.
    #[error("command timed out after {0}s and was killed")]
    Timeout(u64),
    /// The driver cancelled the command mid-run: the context's cancelled
    /// flag flipped before the command finished.
    #[error("cancelled")]
    Cancelled,
    /// The command never started — spawning failed, or the executor's
    /// transport to its host failed.
    #[error("failed to run bash: {0}")]
    Failed(String),
}

/// Runs shell commands for the `bash` tool.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Runs `command` with `bash -c` and `cwd` as working directory,
    /// streaming complete output lines through `progress` as they land
    /// and returning what the command left behind. A command outliving
    /// `timeout` is killed and reported as [`ShellError::Timeout`]; a
    /// flip of `cancelled` mid-run stops the run with
    /// [`ShellError::Cancelled`]. Implementations must not panic and
    /// must not abort: every failure comes back as a [`ShellError`].
    async fn run(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Duration,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Executed, ShellError>;
}

/// The local executor: commands run as `bash -c` child processes on this
/// host, streamed line by line, killed past their timeout (`kill_on_drop`
/// so the dropped child handle kills a still-running process).
#[cfg(feature = "local-shell")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalShell;

#[cfg(feature = "local-shell")]
#[async_trait]
impl ShellExecutor for LocalShell {
    async fn run(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Duration,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Executed, ShellError> {
        use std::process::Stdio;
        use std::sync::atomic::Ordering;

        // kill_on_drop so the timeout dropping the child handle kills the
        // process; without it an expired command would keep running.
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| ShellError::Failed(format!("{err}")))?;
        // One reader per stream, complete lines into one channel: each
        // line streams live through `progress` and lands in its
        // stream's tail for the result.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, false, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, true, tx.clone());
        }
        drop(tx);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(ShellError::Timeout(timeout.as_secs()));
                }
                line = rx.recv() => match line {
                    Some((is_stderr, line)) => {
                        let sink = if is_stderr { &mut stderr } else { &mut stdout };
                        sink.extend_from_slice(line.as_bytes());
                        sink.push(b'\n');
                        progress(&format!("{line}\n"));
                    }
                    // Both streams closed: only the exit status is left.
                    None => break,
                }
            }
            if cancelled.load(Ordering::Relaxed) {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ShellError::Cancelled);
            }
        }
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            // A process that closed its streams but never exits dies with
            // the handle; the caller reads a timeout like any other.
            _ => return Err(ShellError::Timeout(timeout.as_secs())),
        };
        Ok(Executed {
            stdout,
            stderr,
            code: status.code(),
        })
    }
}

/// Reads `stream` line by line into `tx`, tagged with its stream, until
/// EOF — the live side of the command's output. Detached: it ends with
/// the pipe, whether the command finishes, times out, or is killed.
#[cfg(feature = "local-shell")]
fn pump<R>(
    stream: R,
    is_stderr: bool,
    tx: tokio::sync::mpsc::UnboundedSender<(bool, String)>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send((is_stderr, line)).is_err() {
                break; // the resolver is gone; nothing to stream to
            }
        }
    })
}

/// The no-shell placeholder: every command fails in-band. What
/// `ShellTools::new` installs when the build carries no local shell —
/// the model sees the failure, the operator sees the missing setup.
#[cfg(not(feature = "local-shell"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoShell;

#[cfg(not(feature = "local-shell"))]
#[async_trait]
impl ShellExecutor for NoShell {
    async fn run(
        &self,
        _command: &str,
        _cwd: &Path,
        _timeout: Duration,
        _cancelled: &AtomicBool,
        _progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Executed, ShellError> {
        Err(ShellError::Failed(
            "this build has no local shell; install a shell executor \
             (ShellTools::with_executor)"
                .into(),
        ))
    }
}

/// The executor `ShellTools::new` installs: the local process shell where
/// the build has one, the in-band no-shell stub where it does not.
#[cfg(feature = "local-shell")]
pub fn default_executor() -> std::sync::Arc<dyn ShellExecutor> {
    std::sync::Arc::new(LocalShell)
}

/// The executor `ShellTools::new` installs: the local process shell where
/// the build has one, the in-band no-shell stub where it does not.
#[cfg(not(feature = "local-shell"))]
pub fn default_executor() -> std::sync::Arc<dyn ShellExecutor> {
    std::sync::Arc::new(NoShell)
}
