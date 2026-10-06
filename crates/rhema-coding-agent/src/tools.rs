//! The built-in coding tools: file inspection and editing scoped to a
//! workspace, plus an approval-gated shell command.
//!
//! [`CodingTools`] is one [`ToolResolver`], not a registry: the same four
//! specs go to the model every step. Tool paths are resolved against the
//! workspace root, canonicalized, and rejected in-band (`is_error`
//! [`ToolResult`], never a panic — the model reacts in-band, SPEC.md §4)
//! when they escape the root. The two side-effecting tools consult the
//! approval hook installed for the current turn; tool resolution happens
//! synchronously inside a turn, so a per-turn swap of that hook is
//! race-free. No hook installed means gated tools are rejected — the
//! fail-safe default.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

use crate::approval::Approval;

/// `read_file` results are cut at this size, with a notice appended.
const READ_FILE_LIMIT: usize = 100 * 1024;
/// `run_command` output streams are each cut at this size.
const RUN_COMMAND_OUTPUT_LIMIT: usize = 10 * 1024;
/// `run_command` is killed after this long.
const RUN_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The built-in tool set: `read_file`, `list_dir`, `write_file`,
/// `run_command`. Shared behind an `Arc` between the engine's `Agent` and
/// the driver.
pub struct CodingTools {
    /// Canonical workspace root every tool path is scoped to.
    workspace: PathBuf,
    /// The approval hook installed for the current turn; `None` rejects
    /// gated tools.
    pending: Mutex<Option<Arc<dyn Approval>>>,
}

impl CodingTools {
    /// Tools scoped to `workspace`; the path is canonicalized, falling back
    /// to the input if canonicalization fails (canonicalization requires
    /// every component to exist).
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: canonicalized(workspace.into()),
            pending: Mutex::new(None),
        }
    }

    /// The canonical workspace root every tool path is scoped to.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Installs the approval hook consulted by the gated tools. Call once
    /// per turn before stepping; tool resolution cannot overlap another
    /// turn's hook, so the swap is safe.
    pub fn install_approval(&self, approval: Arc<dyn Approval>) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(approval);
    }

    /// Removes the approval hook; gated tools reject until one is installed
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

    fn read_file(&self, call: ToolCall) -> ToolResult {
        let Some(raw) = arg_str(&call.args, "path") else {
            return ToolResult::failure(call.id, "missing required argument: path");
        };
        let path = match resolve_within_workspace(&self.workspace, &raw) {
            Ok(path) => path,
            Err(escaped) => {
                return ToolResult::failure(
                    call.id,
                    format!("workspace escape: {}", escaped.display()),
                );
            }
        };
        match std::fs::read(&path) {
            Ok(bytes) => ToolResult::success(call.id, render_truncated(&bytes, READ_FILE_LIMIT)),
            Err(err) => ToolResult::failure(call.id, format!("read failed: {err}")),
        }
    }

    fn list_dir(&self, call: ToolCall) -> ToolResult {
        let raw = arg_str(&call.args, "path").unwrap_or_default();
        let path = match resolve_within_workspace(&self.workspace, &raw) {
            Ok(path) => path,
            Err(escaped) => {
                return ToolResult::failure(
                    call.id,
                    format!("workspace escape: {}", escaped.display()),
                );
            }
        };
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => return ToolResult::failure(call.id, format!("list_dir failed: {err}")),
        };
        let mut lines: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                // fs::metadata follows symlinks, so a linked directory
                // lists as a directory.
                match std::fs::metadata(entry.path()) {
                    Ok(meta) if meta.is_dir() => format!("dir: {name}"),
                    Ok(meta) => format!("file: {name} ({} bytes)", meta.len()),
                    Err(_) => format!("file: {name}"),
                }
            })
            .collect();
        lines.sort();
        if lines.is_empty() {
            ToolResult::success(call.id, "(empty directory)")
        } else {
            ToolResult::success(call.id, lines.join("\n"))
        }
    }

    async fn write_file(&self, call: ToolCall) -> ToolResult {
        if let Some(rejection) = self.approval_gate(&call) {
            return rejection;
        }
        let Some(raw) = arg_str(&call.args, "path") else {
            return ToolResult::failure(call.id, "missing required argument: path");
        };
        let Some(content) = arg_str(&call.args, "content") else {
            return ToolResult::failure(call.id, "missing required argument: content");
        };
        let path = match resolve_within_workspace(&self.workspace, &raw) {
            Ok(path) => path,
            Err(escaped) => {
                return ToolResult::failure(
                    call.id,
                    format!("workspace escape: {}", escaped.display()),
                );
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                return ToolResult::failure(call.id, format!("write failed: {err}"));
            }
        }
        match std::fs::write(&path, content.as_bytes()) {
            Ok(()) => ToolResult::success(
                call.id,
                format!(
                    "wrote {} ({} bytes)",
                    display_in_workspace(&path, &self.workspace),
                    content.len()
                ),
            ),
            Err(err) => ToolResult::failure(call.id, format!("write failed: {err}")),
        }
    }

    async fn run_command(&self, call: ToolCall) -> ToolResult {
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
        match tokio::time::timeout(RUN_COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let content = format_command_output(&output.status, &output.stdout, &output.stderr);
                if output.status.success() {
                    ToolResult::success(call.id, content)
                } else {
                    ToolResult::failure(call.id, content)
                }
            }
            Ok(Err(err)) => ToolResult::failure(call.id, format!("command failed: {err}")),
            Err(_) => ToolResult::failure(
                call.id,
                format!(
                    "command timed out after {}s and was killed",
                    RUN_COMMAND_TIMEOUT.as_secs()
                ),
            ),
        }
    }
}

#[async_trait]
impl ToolResolver for CodingTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "read_file",
                "Read a text file inside the workspace. Returns the content, \
                 truncated with a notice past 100 KiB.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path relative to the workspace root."
                        }
                    },
                    "required": ["path"]
                }),
            ),
            ToolSpec::new(
                "list_dir",
                "List a directory inside the workspace, one entry per line, \
                 `dir:`/`file:` prefixed, files with byte sizes.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory relative to the workspace root; \
                                            defaults to the root itself."
                        }
                    }
                }),
            ),
            ToolSpec::new(
                "write_file",
                "Create or overwrite a text file inside the workspace, creating \
                 parent directories. Requires user approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path relative to the workspace root."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full file content to write."
                        }
                    },
                    "required": ["path", "content"]
                }),
            ),
            ToolSpec::new(
                "run_command",
                "Run a shell command (`bash -c`) with the workspace as working \
                 directory. Output is capped and the exit status reported; a \
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
            ),
        ]
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        if ctx.cancelled.load(Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        // The name is cloned out because the match arms move `call`.
        let name = call.name.clone();
        match name.as_str() {
            "read_file" => self.read_file(call),
            "list_dir" => self.list_dir(call),
            "write_file" => self.write_file(call).await,
            "run_command" => self.run_command(call).await,
            other => ToolResult::failure(call.id, format!("unknown tool: {other}")),
        }
    }
}

/// Canonicalizes `path`, falling back to the input when it does not exist
/// yet.
fn canonicalized(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Resolves `raw` against `workspace`, lexically and through symlinks, or
/// returns the first resolved path found outside the workspace.
fn resolve_within_workspace(workspace: &Path, raw: &str) -> Result<PathBuf, PathBuf> {
    let candidate = normalize(&workspace.join(raw));
    let resolved = resolve_existing(&candidate);
    if resolved.starts_with(workspace) {
        Ok(resolved)
    } else {
        Err(resolved)
    }
}

/// Lexical normalization: drops `.` components and resolves `..` against
/// the preceding component. Does not touch the filesystem — symlink
/// resolution happens in [`resolve_existing`].
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` at a root cannot pop; keep it so the workspace
                // prefix check rejects the path.
                if !out.pop() {
                    out.push("..");
                }
            }
            component => out.push(component.as_os_str()),
        }
    }
    out
}

/// Canonicalizes `path` if it exists; otherwise canonicalizes the nearest
/// existing ancestor and re-attaches the missing tail, so write targets
/// under a symlinked parent are still checked through the link.
fn resolve_existing(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut ancestor = path.to_path_buf();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&ancestor) {
            let mut out = resolved;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_owned());
                ancestor = parent.to_path_buf();
            }
            // Exhausted the ancestor chain; the lexical form is all we have.
            _ => return path.to_path_buf(),
        }
    }
}

/// Prefer a workspace-relative display path; fall back to the full path.
fn display_in_workspace(path: &Path, workspace: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Lossy text of `bytes`, cut at `limit` on a char boundary with a notice
/// appended when it was cut.
fn render_truncated(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut text = truncate_utf8_lossy(bytes, limit);
    text.push_str("\n[truncated]");
    text
}

fn truncate_utf8_lossy(bytes: &[u8], limit: usize) -> String {
    // Back off over UTF-8 continuation bytes (10xxxxxx) to a char start.
    let mut end = limit.min(bytes.len());
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
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
        render_truncated(stdout, RUN_COMMAND_OUTPUT_LIMIT),
        render_truncated(stderr, RUN_COMMAND_OUTPUT_LIMIT),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dot_and_parent_components() {
        assert_eq!(normalize(Path::new("/ws/a/./b")), PathBuf::from("/ws/a/b"));
        assert_eq!(normalize(Path::new("/ws/a/../b")), PathBuf::from("/ws/b"));
        assert_eq!(
            normalize(Path::new("/ws/../../etc")),
            PathBuf::from("/../etc")
        );
    }

    #[test]
    fn paths_outside_the_workspace_are_rejected_inside_are_resolved() {
        let root = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(root.path()).unwrap();
        assert!(resolve_within_workspace(&workspace, "../outside").is_err());
        assert!(resolve_within_workspace(&workspace, "/etc/hostname").is_err());
        assert!(resolve_within_workspace(&workspace, "sub/../file.txt").is_ok());
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        let bytes = "héllo".as_bytes();
        assert_eq!(truncate_utf8_lossy(bytes, 2), "h");
        assert_eq!(truncate_utf8_lossy(bytes, 3), "hé");
    }
}
