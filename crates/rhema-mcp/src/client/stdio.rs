//! Stdio: the child's stdout is newline-delimited JSON-RPC read under
//! a hard line cap, a garbage line or an oversized one kills the
//! connection, never this process. The child's stderr drains into an
//! 8 KiB ring so a death carries its own last words. Requests
//! multiplex freely; answers route by id.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::{
    ClientError, DeadConnection, MAX_LINE_BYTES, PendingReason, STDERR_TAIL_BYTES, Shared,
    envelope_value,
};
use crate::protocol::{self, Incoming, RequestId, method};

/// What the reader tells the death watcher.
pub(super) enum KillSignal {
    /// A framing violation: terminate the child now.
    Violation,
    /// The child's stdout ended: it is exiting on its own, reap it
    /// and report the death with its stderr tail.
    StreamEnded,
    /// The handshake failed: no client will use this child, kill it.
    Abort,
}

/// Reads stderr into the shared ring, keeping the last
/// [`STDERR_TAIL_BYTES`] bytes.
pub(super) async fn drain_stderr(mut stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut tail = shared.stderr_tail.lock().expect("stderr lock");
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                if excess > 0 {
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// Owns the child's stdin and writes lines to it.
pub(super) async fn writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        if stdin.write_all(b"\n").await.is_err() {
            return;
        }
        if stdin.flush().await.is_err() {
            return;
        }
    }
}

/// Routes stdout lines: responses to the pending map, violations and
/// EOF to the kill switch. Owns no child handles; killing is the
/// death watcher's job.
pub(super) async fn reader_task(
    mut reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    shared: Arc<Shared>,
    kill_tx: mpsc::UnboundedSender<KillSignal>,
) {
    let die = |shared: &Shared, reason: PendingReason| {
        shared.fail_all(ClientError::Dead(DeadConnection::Violation), reason);
    };
    loop {
        match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
            Ok(None) => {
                // EOF: the child is exiting; the watcher reaps it and
                // reports the death with the stderr tail.
                let _ = kill_tx.send(KillSignal::StreamEnded);
                return;
            }
            Ok(Some(raw)) => {
                let line = strip_eol(&raw);
                if line.is_empty() {
                    continue;
                }
                let incoming = match serde_json::from_slice::<Incoming>(line) {
                    Ok(incoming) => incoming,
                    Err(_) => {
                        die(
                            &shared,
                            PendingReason::Protocol("a stdout line was not valid JSON-RPC"),
                        );
                        let _ = kill_tx.send(KillSignal::Violation);
                        return;
                    }
                };
                match incoming {
                    Incoming::Response(response) => {
                        if let Err(violation) = route_response(&shared, response) {
                            die(&shared, PendingReason::Protocol(violation));
                            let _ = kill_tx.send(KillSignal::Violation);
                            return;
                        }
                    }
                    Incoming::Notification(notification) => {
                        route_notification(&shared, notification)
                    }
                }
            }
            Err(LineError::TooLong) => {
                die(
                    &shared,
                    PendingReason::Protocol("a stdout line exceeded the frame cap"),
                );
                let _ = kill_tx.send(KillSignal::Violation);
                return;
            }
            Err(LineError::Io) => {
                die(&shared, PendingReason::Protocol("the stdout pipe failed"));
                let _ = kill_tx.send(KillSignal::Violation);
                return;
            }
        }
    }
}

/// Sends a response to whoever waits on its id. Unmatched responses
/// (late, or to an abandoned request) drop silently — the abandoned
/// caller has already moved on. A response naming an id this client
/// never minted is different: ids are handed out in order, so the
/// mint mark (`next_id`) bounds everything ever asked, and such a
/// response can never match anyone, late or not. It kills the
/// connection, the stdio counterpart of the HTTP transport's loud
/// failure for the same server bug (the SSE stream ends without an
/// answer); dropped instead, every pending caller would strand until
/// its own budget ran out. The error is the violation text.
fn route_response(shared: &Shared, response: protocol::Response) -> Result<(), &'static str> {
    let Some(id) = echoed_numeric(&response.id) else {
        return Err(NEVER_MINTED);
    };
    if id >= shared.next_id.load(Ordering::SeqCst) {
        return Err(NEVER_MINTED);
    }
    let sender = shared.pending.lock().expect("pending lock").remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(envelope_value(response));
    }
    Ok(())
}

/// The violation text for a response naming an id this client never
/// minted.
const NEVER_MINTED: &str = "a response echoed an id this client never minted";

/// The request an echoed id names, in either shape: this client
/// always mints numeric ids, but JSON-RPC lets a server echo them as
/// strings, and the decimal number is the same request either way.
/// Mint side and route side agree on the number.
pub(super) fn echoed_numeric(id: &RequestId) -> Option<u64> {
    match id {
        RequestId::Number(n) => Some(*n),
        RequestId::Text(text) => text.parse().ok(),
    }
}

/// Routes server notifications: only `notifications/cancelled` means
/// anything to this subset; everything else (progress, log) is data
/// this layer does not surface. An unmatched cancellation (or one
/// naming an unknown id) is a no-op, advisory at most.
fn route_notification(shared: &Shared, notification: protocol::Notification) {
    if notification.method != method::CANCELLED {
        return;
    }
    let Some(params) = notification.params else {
        return;
    };
    let Ok(cancelled) = serde_json::from_value::<protocol::CancelledParams>(params) else {
        return;
    };
    let Some(id) = echoed_numeric(&cancelled.request_id) else {
        return;
    };
    if let Some(sender) = shared.pending.lock().expect("pending lock").remove(&id) {
        let _ = sender.send(Err(ClientError::Cancelled));
    }
}

/// Why a line read failed. The pipe's own error detail is dropped:
/// "the stdout pipe failed" is all a caller can act on.
enum LineError {
    /// The line outgrew the frame cap.
    TooLong,
    /// The pipe itself failed.
    Io,
}

/// Reads one `\n`-terminated line with a hard cap: the buffered
/// readers' own line methods grow without bound on a hostile stream.
/// A final line with no trailing newline still counts as a line.
async fn read_line_capped<R: tokio::io::AsyncBufRead + std::marker::Unpin>(
    reader: &mut R,
    cap: usize,
) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    loop {
        let (done, used) = match reader.fill_buf().await.map_err(|_| LineError::Io)? {
            // EOF with nothing buffered: a clean end of stream.
            [] => {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(line))
                };
            }
            buffered => match buffered.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    if line.len() + pos + 1 > cap {
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(&buffered[..=pos]);
                    (true, pos + 1)
                }
                None => {
                    if line.len() + buffered.len() > cap {
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(buffered);
                    (false, buffered.len())
                }
            },
        };
        reader.consume(used);
        if done {
            return Ok(Some(line));
        }
    }
}

/// Strips a trailing `\n` or `\r\n`.
fn strip_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}
