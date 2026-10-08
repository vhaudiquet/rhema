//! The built-in tool: one approval-gated shell, scoped to a workspace.
//!
//! [`CodingTools`] is one [`ToolResolver`], not a registry: the same spec
//! goes to the model every step. `bash` runs its command with `bash -c`
//! and the workspace root as working directory — reading, listing, and
//! editing are shell work, not separate tools. The output streams are
//! capped and the command killed after a timeout; the exit code rides on
//! the result for the UI. Every call consults the approval hook installed
//! for the current turn; tool resolution happens synchronously inside a
//! turn, so a per-turn swap of that hook is race-free. No hook installed
//! means calls are rejected — the fail-safe default.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

use crate::approval::Approval;

/// Command output streams are each cut at this size — in bytes and in
/// lines, whichever is hit first — keeping the tail: errors and final
/// results live at the end of a command's output.
const BASH_OUTPUT_LIMIT: usize = 50 * 1024;
const BASH_OUTPUT_LINES: usize = 2000;
/// A command is killed after this long.
const BASH_TIMEOUT: Duration = Duration::from_secs(120);

/// The built-in tool set: `bash`. Shared behind an `Arc` between the
/// engine's `Agent` and the driver.
pub struct CodingTools {
    /// Working directory every command starts in.
    workspace: PathBuf,
    /// The approval hook installed for the current turn; `None` rejects
    /// calls.
    pending: Mutex<Option<Arc<dyn Approval>>>,
}

impl CodingTools {
    /// Tools rooted at `workspace`; the path is canonicalized, falling back
    /// to the input if canonicalization fails (canonicalization requires
    /// every component to exist).
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: canonicalized(workspace.into()),
            pending: Mutex::new(None),
        }
    }

    /// The working directory every command starts in.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Installs the approval hook consulted by the tool. Call once per
    /// turn before stepping; tool resolution cannot overlap another
    /// turn's hook, so the swap is safe.
    pub fn install_approval(&self, approval: Arc<dyn Approval>) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(approval);
    }

    /// Removes the approval hook; calls reject until one is installed
    /// again.
    pub fn clear_approval(&self) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// `None` to proceed, `Some` a rejection result to return instead.
    fn approval_gate(&self, call: &ToolCall) -> Option<ToolResult> {
        let approved = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|approval| approval.approve(call));
        if approved {
            None
        } else {
            Some(ToolResult::failure(&call.id, "rejected by user approval"))
        }
    }

    async fn bash(&self, call: ToolCall) -> ToolResult {
        if let Some(rejection) = self.approval_gate(&call) {
            return rejection;
        }
        let Some(command) = arg_str(&call.args, "command") else {
            return ToolResult::failure(call.id, "missing required argument: command");
        };
        // kill_on_drop so the timeout dropping the child handle kills the
        // process; without it an expired command would keep running.
        let child = match tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&self.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                return ToolResult::failure(call.id, format!("failed to spawn bash: {err}"));
            }
        };
        match tokio::time::timeout(BASH_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let content = format_command_output(&output.status, &output.stdout, &output.stderr);
                let result = if output.status.success() {
                    ToolResult::success(call.id, content)
                } else {
                    ToolResult::failure(call.id, content)
                };
                match output.status.code() {
                    Some(code) => result.with_code(code),
                    None => result,
                }
            }
            Ok(Err(err)) => ToolResult::failure(call.id, format!("command failed: {err}")),
            Err(_) => ToolResult::failure(
                call.id,
                format!(
                    "command timed out after {}s and was killed",
                    BASH_TIMEOUT.as_secs()
                ),
            ),
        }
    }
}

#[async_trait]
impl ToolResolver for CodingTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "bash",
            "Run a shell command (`bash -c`) with the workspace root as \
             working directory. Reading, listing, and editing files are \
             shell work. Output is capped and the exit status reported; a \
             non-zero exit comes back as an error. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run."
                    }
                },
                "required": ["command"]
            }),
        )]
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        if ctx.cancelled.load(Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        // The name is cloned out because the match arms move `call`.
        let name = call.name.clone();
        match name.as_str() {
            "bash" => self.bash(call).await,
            other => ToolResult::failure(call.id, format!("unknown tool: {other}")),
        }
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Canonicalizes `path`, falling back to the input when it does not exist
/// yet.
fn canonicalized(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Lossy text of `bytes`, tail-kept at `limit` bytes and `max_lines`
/// lines (whichever is hit first): the elided head is counted in a
/// marker, and the byte cut lands on a line start so the first kept row
/// is whole. A single line longer than the whole byte budget is kept
/// partially, past the marker.
fn render_truncated(bytes: &[u8], limit: usize, max_lines: usize) -> String {
    let line_count = bytes.iter().filter(|&&byte| byte == b'\n').count();
    if bytes.len() <= limit && line_count <= max_lines {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let byte_cut = bytes.len().saturating_sub(limit);
    let byte_cut = match bytes[..byte_cut].iter().rposition(|&byte| byte == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    // The line cut: the start of the row that begins the last
    // `max_lines` rows.
    let mut line_cut = 0usize;
    let mut seen = 0usize;
    for (index, &byte) in bytes.iter().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == max_lines {
                line_cut = index + 1;
                break;
            }
        }
    }
    let cut = byte_cut.max(line_cut);
    let mut text = format!("[output truncated — {} bytes elided]\n", cut);
    text.push_str(&String::from_utf8_lossy(&bytes[cut..]));
    text
}

fn format_command_output(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let code = match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    };
    format!(
        "{code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        render_truncated(stdout, BASH_OUTPUT_LIMIT, BASH_OUTPUT_LINES),
        render_truncated(stderr, BASH_OUTPUT_LIMIT, BASH_OUTPUT_LINES),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_tail_and_counts_the_elided_head() {
        let bytes = b"head line\nerror: the real failure\n".to_vec();
        let text = render_truncated(&bytes, 10, 2000);
        assert!(text.starts_with("[output truncated"), "{text}");
        assert!(text.contains("error: the real failure"), "{text}");
        // The cut lands on a line start: the first kept row is whole.
        assert!(!text.contains("head line"), "{text}");
    }

    #[test]
    fn the_line_limit_cuts_more_rows_than_the_byte_limit() {
        let bytes = (0..3000).map(|i| format!("row {i}\n")).collect::<String>();
        let text = render_truncated(bytes.as_bytes(), 50 * 1024, 2000);
        // The last 2000 rows minus one: the cut lands after row 1000's
        // newline, so rows 1001..2999 survive.
        assert!(text.starts_with("[output truncated — "), "{text}");
        assert!(text.contains("row 1001\n"), "{text}");
        assert!(text.contains("row 2999\n"), "{text}");
        assert!(!text.contains("row 1000\n"), "{text}");
        assert!(!text.contains("row 999\n"), "{text}");
    }

    #[test]
    fn output_within_the_limits_passes_untouched() {
        assert_eq!(render_truncated(b"clean\n", 10, 2000), "clean\n");
    }
}
