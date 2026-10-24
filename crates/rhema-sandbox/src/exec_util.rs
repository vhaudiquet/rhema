//! The machinery the process-spawning backends share.
//!
//! `rhema-sandbox-local`, `rhema-sandbox-docker`, and
//! `rhema-sandbox-ssh` each spawn one child and read two pipes; this
//! module is the half they have in common — the stream pump with its
//! capped tails, the cancel loop that kills the local child, the
//! `timeout` exit codes, the fs routes of the fs-sharing family, and
//! the textual path guard for the backends whose paths another machine
//! interprets. Not private to this crate: it is what the sibling
//! backend crates build on. The seam itself stays the
//! [`Sandbox`](crate::Sandbox) trait, not this wiring.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::fsroot::FsRoot;
use crate::{ExecOutcome, ExecRequest, SandboxError};

/// The stream tail a finished command keeps, the shim's
/// `MAX_STREAM_BYTES`: a runaway command cannot flood memory, and the
/// tool applies its display caps again on top (spec §3 — caps are
/// per-implementation policy; this implementation keeps the shim's).
pub const MAX_STREAM_BYTES: usize = 50 * 1024;

/// How often the cancelled flag is sampled while output is quiet. An
/// `AtomicBool` carries no wakeup, so a silent command would otherwise
/// never observe a flip; this tick is the cancellation check, not a
/// timer kill — the budget belongs to the `timeout` prefix.
const CANCEL_TICK: Duration = Duration::from_millis(100);

/// The file the tarball seed is staged under before `tar` unpacks it.
/// The name on the wire is diagnostics only, so the staging file has
/// one fixed name nothing in the seed can steer.
pub const STAGING: &str = ".rhema-seed.tar.gz";

/// Runs one spawned child to completion: both pipes pumped (complete
/// lines live through `progress`, a capped tail kept for each), a
/// cancel observed before the start and between chunks with the local
/// child killed on it, and the exit read through the `timeout` prefix's
/// codes — 124 at the budget, 137 after `--kill-after` escalates — as
/// [`SandboxError::Timeout`]. Any other exit, nonzero included, is the
/// command's own result and rides to the caller.
pub async fn run_child(
    mut child: tokio::process::Child,
    request: &ExecRequest,
    cancelled: &AtomicBool,
    progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<ExecOutcome, SandboxError> {
    // One reader per stream, complete lines into one channel: each
    // line streams live through `progress`, and each reader keeps
    // its own capped tail for the result.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_tail = tokio::spawn(pump(child.stdout.take(), false, tx.clone()));
    let stderr_tail = tokio::spawn(pump(child.stderr.take(), true, tx.clone()));
    drop(tx);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some((_, line)) => {
                    progress(&format!("{line}\n"));
                    if cancelled.load(Ordering::Relaxed) {
                        kill(&mut child).await;
                        return Err(SandboxError::Failed("cancelled".into()));
                    }
                }
                // Both streams closed: only the exit status is left.
                None => break,
            },
            _ = tokio::time::sleep(CANCEL_TICK) => {
                if cancelled.load(Ordering::Relaxed) {
                    kill(&mut child).await;
                    return Err(SandboxError::Failed("cancelled".into()));
                }
            }
        }
    }
    // The command is done: the model sees its true result whatever
    // the turn decided meanwhile, so the flag is not rechecked.
    let stdout = stdout_tail.await.unwrap_or_default();
    let stderr = stderr_tail.await.unwrap_or_default();
    let status = child
        .wait()
        .await
        .map_err(|err| SandboxError::Unavailable(format!("waiting for the command: {err}")))?;
    let code = status.code();
    if code == Some(124) || code == Some(137) {
        return Err(SandboxError::Timeout(request.timeout.as_secs()));
    }
    Ok(ExecOutcome {
        stdout,
        stderr,
        code,
    })
}

/// Reads `stream` line by line, sending every complete line — tagged
/// with its stream — through `tx`, and keeping the capped tail for the
/// result. A detached task: it ends with the pipe, whether the command
/// finishes or is killed, and its tail joins at the end.
async fn pump<R>(
    stream: Option<R>,
    is_stderr: bool,
    tx: tokio::sync::mpsc::UnboundedSender<(bool, String)>,
) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let mut tail = Vec::new();
    if let Some(stream) = stream {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send((is_stderr, line.clone()));
            append_capped(&mut tail, line.as_bytes());
            append_capped(&mut tail, b"\n");
        }
    }
    tail
}

/// Appends `bytes` to `tail`, keeping at most the last
/// [`MAX_STREAM_BYTES`] — the cap is applied as the bytes land, so a
/// runaway command never holds more than the tail.
fn append_capped(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > MAX_STREAM_BYTES {
        let excess = tail.len() - MAX_STREAM_BYTES;
        tail.drain(..excess);
    }
}

/// Kills the child now and reaps it — the explicit half of
/// `kill_on_drop`, so a cancelled run's command does not outlive the
/// call.
async fn kill(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Guards a path some other filesystem interprets — a container's, a
/// remote's. There is no local root to resolve against, so the check
/// is textual: the path must be absolute and may carry no `..`
/// component (the empty path is refused too — there is no root here
/// for it to mean). The paths are model-influenced; this is the guard.
pub fn check_remote_path(path: &str) -> Result<(), SandboxError> {
    if !path.starts_with('/') {
        return Err(SandboxError::Failed(format!(
            "sandbox path must be absolute: {path:?}"
        )));
    }
    if path.split('/').any(|name| name == "..") {
        return Err(SandboxError::Failed(format!(
            "sandbox path escapes the workspace: {path:?}"
        )));
    }
    Ok(())
}

/// Shell-quotes one argument, single-quote style — the shim's
/// `shquote`, restated wherever an argument crosses a shell: the seed
/// command lines, and every remote operand `ssh` hands its far-end
/// shell to reparse.
pub fn shquote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The fs-sharing family's read route: map through the root, read the
/// bytes. A miss is an in-band failure naming the sandbox path.
pub async fn read_through(root: &FsRoot, path: &str) -> Result<Vec<u8>, SandboxError> {
    let mapped = root.map(path)?;
    tokio::fs::read(&mapped)
        .await
        .map_err(|err| SandboxError::Failed(format!("{path}: {err}")))
}

/// The fs-sharing family's write route: map through the root, make the
/// parent directories — the shim's put creates them, and the skills
/// seeding writes two levels deep — then write the bytes.
pub async fn write_through(root: &FsRoot, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
    let mapped = root.map(path)?;
    if let Some(parent) = mapped.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| SandboxError::Failed(format!("{path}: {err}")))?;
    }
    tokio::fs::write(&mapped, contents)
        .await
        .map_err(|err| SandboxError::Failed(format!("{path}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_refused() {
        for path in ["", "workspace", "workspace/x", "./x"] {
            let error = check_remote_path(path).expect_err("the relative path is refused");
            assert!(
                matches!(error, SandboxError::Failed(_)),
                "the refusal is an in-band failure, not a crash: {error:?}"
            );
        }
    }

    #[test]
    fn a_dot_dot_component_is_refused_wherever_it_sits() {
        for path in ["..", "/..", "/../etc", "/a/../b", "/a/b/../../c", "/a/.."] {
            let error = check_remote_path(path).expect_err("the climb is refused");
            assert!(
                matches!(error, SandboxError::Failed(_)),
                "the refusal is an in-band failure, not a crash: {error:?}"
            );
        }
    }

    #[test]
    fn plain_absolute_paths_pass() {
        for path in [
            "/",
            "/workspace",
            "/workspace/deep/note.md",
            "/a/..b",
            "/a/...",
        ] {
            check_remote_path(path).expect("the path passes the guard");
        }
    }

    #[test]
    fn quoting_survives_embedded_single_quotes() {
        assert_eq!(shquote("plain"), "'plain'");
        assert_eq!(shquote("a'b"), "'a'\\''b'");
        assert_eq!(shquote(""), "''");
    }

    #[test]
    fn the_tail_is_capped_as_the_bytes_land() {
        let mut tail = Vec::new();
        for _ in 0..4 {
            append_capped(&mut tail, &[b'x'; MAX_STREAM_BYTES]);
        }
        assert_eq!(tail.len(), MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn the_pump_keeps_a_capped_tail_of_the_lines() {
        let payload: Vec<u8> = b"first\nsecond\n".to_vec();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tail = pump(Some(&payload[..]), false, tx).await;
        assert_eq!(tail, b"first\nsecond\n".as_slice());
    }

    #[tokio::test]
    async fn an_absent_stream_pumps_to_an_empty_tail() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tail = pump::<&[u8]>(None, false, tx).await;
        assert!(tail.is_empty());
    }
}
