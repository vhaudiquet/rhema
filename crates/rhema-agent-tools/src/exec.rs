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

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(feature = "local-shell")]
use std::sync::Arc;
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

/// How often the cancelled flag is sampled while a command's output is
/// quiet — an `AtomicBool` carries no wakeup, so a silent command would
/// otherwise never observe a flip until its deadline.
#[cfg(feature = "local-shell")]
const CANCEL_TICK: Duration = Duration::from_millis(100);

/// One stream's capture window: everything the result renderer can
/// show (`BASH_OUTPUT_LIMIT` of bytes, or the last `BASH_OUTPUT_LINES`
/// rows — whichever cuts later) plus slack for line alignment, so the
/// truncation the model sees matches an uncaptured run's. Bounds the
/// memory a chatty command can pin: without it, `yes` streams through
/// the channel into these buffers until the deadline.
#[cfg(feature = "local-shell")]
const CAPTURE_WINDOW: usize = 50 * 1024 + 1024 * 1024;

/// The overshoot an eviction waits for: draining per line would shift
/// the retained window on every line of a chatty stream (quadratic);
/// waiting for a quarter-window of overshoot makes the cost
/// amortized-linear. Peak residency is the window plus this.
#[cfg(feature = "local-shell")]
const CAPTURE_EVICT: usize = 256 * 1024;

/// The local executor: commands run as `bash -c` child processes on this
/// host, streamed line by line, killed past their timeout (`kill_on_drop`
/// so the dropped child handle kills a still-running process).
#[cfg(feature = "local-shell")]
#[derive(Debug, Clone)]
pub struct LocalShell {
    /// Extra environment every command runs with, layered over the
    /// inherited one — how a frontend provisions its agent's tools (a
    /// daemon URL, a scoped token, a PATH addition) without touching
    /// the process environment.
    env: Arc<[(String, String)]>,
}

#[cfg(feature = "local-shell")]
impl Default for LocalShell {
    fn default() -> Self {
        Self { env: Arc::from([]) }
    }
}

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
            .envs(
                self.env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
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
                        // Bound the capture: the renderer reads only
                        // the tail, so past the window drop whole head
                        // lines down to it. The cut lands on a newline
                        // when there is one, keeping the retained bytes
                        // line-framed for the renderer's tail logic.
                        if sink.len() >= CAPTURE_WINDOW + CAPTURE_EVICT {
                            let keep_from = sink.len() - CAPTURE_WINDOW;
                            let boundary = sink[keep_from..]
                                .iter()
                                .position(|&byte| byte == b'\n')
                                .map_or(keep_from, |position| keep_from + position + 1);
                            sink.drain(..boundary);
                        }
                        progress(&format!("{line}\n"));
                    }
                    // Both streams closed: only the exit status is left.
                    None => break,
                },
                // The cancelled flag carries no wakeup: without this tick
                // a command that streams nothing would not observe a flip
                // until its deadline. The tick is the cancellation check,
                // not a timer kill — the budget belongs to the deadline.
                _ = tokio::time::sleep(CANCEL_TICK) => {
                    if cancelled.load(Ordering::Relaxed) {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(ShellError::Cancelled);
                    }
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
    std::sync::Arc::new(LocalShell::default())
}

/// The default executor with `env` layered over every command's
/// environment — the provisioning seam a frontend's agent tools ride
/// in on. A build without the `local-shell` feature has no commands to
/// provision; the stub ignores `env`.
#[cfg(feature = "local-shell")]
pub fn default_executor_with(env: &BTreeMap<String, String>) -> std::sync::Arc<dyn ShellExecutor> {
    std::sync::Arc::new(LocalShell {
        env: Arc::from(
            env.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>(),
        ),
    })
}

/// The stub counterpart of [`default_executor_with`]: nothing runs, so
/// there is nothing to provision.
#[cfg(not(feature = "local-shell"))]
pub fn default_executor_with(_env: &BTreeMap<String, String>) -> std::sync::Arc<dyn ShellExecutor> {
    std::sync::Arc::new(NoShell)
}

/// The executor `ShellTools::new` installs: the local process shell where
/// the build has one, the in-band no-shell stub where it does not.
#[cfg(not(feature = "local-shell"))]
pub fn default_executor() -> std::sync::Arc<dyn ShellExecutor> {
    std::sync::Arc::new(NoShell)
}

#[cfg(all(test, feature = "local-shell"))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn silent(_line: &str) {}

    #[tokio::test]
    async fn a_chatty_command_cannot_grow_the_capture_without_bound() {
        let shell = LocalShell::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        // ~1.8 MB of stdout: far past the capture window, far under
        // the deadline, so the cap — not the timeout — is what held.
        let executed = shell
            .run(
                "seq 1 300000",
                Path::new("/tmp"),
                Duration::from_secs(60),
                &cancelled,
                &mut silent,
            )
            .await
            .unwrap();
        assert_eq!(executed.code, Some(0));
        assert!(
            executed.stdout.len() < CAPTURE_WINDOW + CAPTURE_EVICT,
            "capture must stay bounded, got {} bytes",
            executed.stdout.len()
        );
        // The tail survives: the last lines are what the model reads.
        let tail = String::from_utf8_lossy(&executed.stdout);
        assert!(
            tail.ends_with("300000\n"),
            "tail lost: …{}",
            &tail[tail.len().saturating_sub(40)..]
        );
    }
}
