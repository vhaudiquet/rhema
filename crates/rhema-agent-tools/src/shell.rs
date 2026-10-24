//! The built-in tool: one approval-gated shell, scoped to a workspace.
//!
//! [`ShellTools`] is one [`ToolResolver`], not a registry: the same spec
//! goes to the model every step. `bash` runs its command with `bash -c`
//! and the workspace root as working directory — reading, listing, and
//! editing are shell work, not separate tools. The output streams are
//! capped and the command killed after a timeout; the exit code rides on
//! the result for the UI. Every call consults the approval hook installed
//! for the current turn; tool resolution happens synchronously inside a
//! turn, so a per-turn swap of that hook is race-free. No hook installed
//! means calls are rejected — the fail-safe default.
//!
//! Where the command actually runs is the [`ShellExecutor`] seam
//! ([`crate::exec`]): this host's `bash -c` child processes by default, a
//! remote sandbox when a frontend brings one. The spec, the gate, the
//! caps, and the error shaping stay here, so a command's tool result
//! reads the same from either side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use rhema_core::{
    OutputOnly, ToolCall, ToolContext, ToolProgress, ToolResolver, ToolResult, ToolSpec,
};

use crate::approval::Approval;
use crate::exec::{Executed, ShellError, ShellExecutor};

/// Command output streams are each cut at this size — in bytes and in
/// lines, whichever is hit first — keeping the tail: errors and final
/// results live at the end of a command's output.
const BASH_OUTPUT_LIMIT: usize = 50 * 1024;
const BASH_OUTPUT_LINES: usize = 2000;
/// A command is killed after this long.
const BASH_TIMEOUT: Duration = Duration::from_secs(120);

/// The built-in tool set: `bash`. Shared behind an `Arc` between the
/// engine's `Agent` and the driver.
pub struct ShellTools {
    /// Working directory every command starts in.
    workspace: PathBuf,
    /// Where commands run: this host's processes by default, a remote
    /// host when the frontend installed one.
    exec: Arc<dyn ShellExecutor>,
    /// The approval hook installed for the current turn; `None` rejects
    /// calls.
    pending: Mutex<Option<Arc<dyn Approval>>>,
}

impl ShellTools {
    /// The specs the built-in tool set advertises. Fixed at compile time
    /// and independent of the instance: the system-prompt renderer names
    /// the same tools the resolver offers, so the prompt and the API's
    /// tools array cannot drift.
    pub fn builtin_specs() -> Vec<ToolSpec> {
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

    /// Tools rooted at `workspace`, commands running on this host; the
    /// path is canonicalized, falling back to the input if
    /// canonicalization fails (canonicalization requires every component
    /// to exist). A build without the `local-shell` feature installs the
    /// no-shell executor: every command fails in-band until a real one is
    /// installed.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self::with_executor(workspace, crate::exec::default_executor())
    }

    /// Tools rooted at `workspace` whose commands run with `env`
    /// layered over the inherited environment — how a frontend hands
    /// its agent's tools a connection (a daemon URL, a scoped token, a
    /// PATH addition) without touching the process environment. A build
    /// without the `local-shell` feature installs the no-shell executor
    /// either way: the env has nothing to ride on.
    pub fn with_env(workspace: impl Into<PathBuf>, env: &BTreeMap<String, String>) -> Self {
        Self::with_executor(workspace, crate::exec::default_executor_with(env))
    }

    /// Tools rooted at `workspace` whose commands run on `exec` — the
    /// seam a remote-shell frontend comes in through.
    pub fn with_executor(workspace: impl Into<PathBuf>, exec: Arc<dyn ShellExecutor>) -> Self {
        Self {
            workspace: canonicalized(workspace.into()),
            exec,
            pending: Mutex::new(None),
        }
    }

    /// The working directory every command starts in.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The executor commands run on. A resolver that runs its own inner
    /// `ShellTools` — the subagents extension — builds the inner one on
    /// the same executor, so subagents land where the turn's commands
    /// land.
    pub fn executor(&self) -> Arc<dyn ShellExecutor> {
        self.exec.clone()
    }

    /// Installs the approval hook consulted by the tool. Call once per
    /// turn before stepping; tool resolution cannot overlap another
    /// turn's hook, so the swap is safe.
    pub fn install_approval(&self, approval: Arc<dyn Approval>) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(approval);
    }

    /// The approval hook installed for the current turn, if any. A
    /// resolver that runs its own inner `ShellTools` — the subagents
    /// extension — reads this to inherit the turn's policy rather than
    /// holding a hook of its own.
    pub fn current_approval(&self) -> Option<Arc<dyn Approval>> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

    async fn bash(
        &self,
        call: ToolCall,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
    ) -> ToolResult {
        if let Some(rejection) = self.approval_gate(&call) {
            return rejection;
        }
        let Some(command) = arg_str(&call.args, "command") else {
            return ToolResult::failure(call.id, "missing required argument: command");
        };
        // The executor's seam speaks raw output chunks; the status half of
        // the progress channel has no command-side counterpart.
        match self
            .exec
            .run(
                &command,
                &self.workspace,
                BASH_TIMEOUT,
                cancelled,
                &mut |chunk: &str| progress.output(chunk),
            )
            .await
        {
            Ok(executed) => finished(call.id, executed),
            Err(ShellError::Timeout(secs)) => ToolResult::failure(
                call.id,
                format!("command timed out after {secs}s and was killed"),
            ),
            Err(ShellError::Cancelled) => ToolResult::failure(call.id, "cancelled"),
            Err(err) => ToolResult::failure(call.id, err.to_string()),
        }
    }
}

/// The tool result for a command that reached an exit: the output, the
/// success flag from the code, and the code itself for the UI.
fn finished(id: String, executed: Executed) -> ToolResult {
    let content = format_command_output(executed.code, &executed.stdout, &executed.stderr);
    let result = if executed.code == Some(0) {
        ToolResult::success(id, content)
    } else {
        ToolResult::failure(id, content)
    };
    match executed.code {
        Some(code) => result.with_code(code),
        None => result,
    }
}

#[async_trait]
impl ToolResolver for ShellTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Self::builtin_specs()
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        // A fn item, not a closure: its lifetime-generic signature is
        // what the higher-ranked progress bound needs.
        self.resolve_streaming(call, ctx, &mut OutputOnly(silent))
            .await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut dyn ToolProgress,
    ) -> ToolResult {
        if ctx.cancelled.load(Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        // The name is cloned out because the match arms move `call`.
        let name = call.name.clone();
        match name.as_str() {
            "bash" => self.bash(call, &ctx.cancelled, progress).await,
            other => ToolResult::failure(call.id, format!("unknown tool: {other}")),
        }
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The no-op progress: the plain `resolve` path runs the same streaming
/// code with nobody listening.
fn silent(_: &str) {}

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

fn format_command_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let code = match code {
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
mod truncation_tests {
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

    #[test]
    fn a_signalled_command_reports_termination_not_an_exit_code() {
        let text = format_command_output(None, b"partial\n", b"");
        assert!(text.starts_with("terminated by signal"), "{text}");
    }
}

#[cfg(test)]
mod executor_tests {
    use super::*;
    use crate::approval::AllowAll;
    use serde_json::json;
    use std::sync::Mutex;

    /// An executor that answers from a script, recording the commands it
    /// was handed: the delegation tests run without spawning anything.
    struct Scripted {
        outcome: Result<Executed, ShellError>,
        commands: Mutex<Vec<String>>,
        streamed: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn ok(stdout: &str) -> Self {
            Self {
                outcome: Ok(Executed {
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                    code: Some(0),
                }),
                commands: Mutex::new(Vec::new()),
                streamed: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ShellExecutor for Scripted {
        async fn run(
            &self,
            command: &str,
            _cwd: &Path,
            _timeout: Duration,
            _cancelled: &AtomicBool,
            progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> Result<Executed, ShellError> {
            self.commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command.to_owned());
            progress("streamed line\n");
            self.streamed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push("streamed line\n".to_owned());
            self.outcome.clone()
        }
    }

    fn call(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            args: json!({ "command": command }),
            provider_data: Value::Null,
        }
    }

    fn streamed(executor: &Scripted) -> Vec<String> {
        executor
            .streamed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[tokio::test]
    async fn a_result_streams_progress_and_carries_the_output() {
        let executor = Arc::new(Scripted::ok("out\n"));
        let tools = ShellTools::with_executor("/tmp", executor.clone());
        tools.install_approval(Arc::new(AllowAll));
        let mut progress = OutputOnly(silent);
        let result = tools
            .resolve_streaming(call("echo hi"), &ToolContext::default(), &mut progress)
            .await;
        assert!(!result.is_error);
        assert_eq!(result.code, Some(0));
        assert!(
            result.content.contains("--- stdout ---\nout\n"),
            "{result:?}"
        );
        // The command reached the executor verbatim, and the executor's
        // progress rode through to the resolver's caller.
        assert_eq!(executor.commands.lock().unwrap().as_slice(), ["echo hi"]);
        assert_eq!(streamed(&executor), vec!["streamed line\n"]);
    }

    #[tokio::test]
    async fn every_shell_error_lands_as_an_in_band_failure() {
        let cases: Vec<(ShellError, &str)> = vec![
            (
                ShellError::Timeout(120),
                "command timed out after 120s and was killed",
            ),
            (ShellError::Cancelled, "cancelled"),
            (
                ShellError::Failed("host unreachable".into()),
                "failed to run bash: host unreachable",
            ),
        ];
        for (outcome, expected) in cases {
            let executor = Arc::new(Scripted {
                outcome: Err(outcome),
                commands: Mutex::new(Vec::new()),
                streamed: Mutex::new(Vec::new()),
            });
            let tools = ShellTools::with_executor("/tmp", executor);
            tools.install_approval(Arc::new(AllowAll));
            let result = tools.resolve(call("x"), &ToolContext::default()).await;
            assert!(result.is_error, "{expected}");
            assert_eq!(result.content, expected);
        }
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_an_error_result_with_the_code() {
        let executor = Arc::new(Scripted {
            outcome: Ok(Executed {
                stdout: b"boom\n".to_vec(),
                stderr: b"trace\n".to_vec(),
                code: Some(3),
            }),
            commands: Mutex::new(Vec::new()),
            streamed: Mutex::new(Vec::new()),
        });
        let tools = ShellTools::with_executor("/tmp", executor);
        tools.install_approval(Arc::new(AllowAll));
        let result = tools.resolve(call("exit 3"), &ToolContext::default()).await;
        assert!(result.is_error);
        assert_eq!(result.code, Some(3));
        assert!(result.content.contains("exit code 3"));
        assert!(result.content.contains("--- stderr ---\ntrace\n"));
    }

    #[tokio::test]
    async fn the_inner_shell_shares_the_executor_of_its_parent() {
        // The subagents extension builds its inner shell off this
        // accessor; the command must land where the parent's do.
        let executor = Arc::new(Scripted::ok(""));
        let tools = ShellTools::with_executor("/tmp", executor);
        let inner = ShellTools::with_executor(tools.workspace().to_path_buf(), tools.executor());
        assert_eq!(inner.workspace(), tools.workspace());
        assert!(Arc::ptr_eq(&inner.executor(), &tools.executor()));
    }
}

#[cfg(all(test, feature = "local-shell"))]
mod local_process_tests {
    use super::*;
    use crate::approval::AllowAll;
    use serde_json::json;

    fn call(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            args: json!({ "command": command }),
            provider_data: Value::Null,
        }
    }

    #[tokio::test]
    async fn a_command_streams_its_lines_before_its_result() {
        let tools = ShellTools::new(std::env::temp_dir());
        tools.install_approval(Arc::new(AllowAll));
        let streamed = Arc::new(Mutex::new(Vec::new()));
        let collector = streamed.clone();
        let mut progress = OutputOnly(move |chunk: &str| {
            collector
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(chunk.to_owned());
        });
        let result = tools
            .resolve_streaming(
                call("printf 'one\\ntwo\\n'; echo three"),
                &ToolContext::default(),
                &mut progress,
            )
            .await;

        // Every line rode the progress live, newline included.
        assert_eq!(
            *streamed.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["one\n".to_owned(), "two\n".to_owned(), "three\n".to_owned()]
        );
        // The result still carries the whole output, stdout and stderr
        // in their own sections.
        assert!(!result.is_error);
        assert!(result.content.contains("--- stdout ---\n"));
        assert!(result.content.contains("one\ntwo\nthree"));
        tools.clear_approval();
    }

    #[tokio::test]
    async fn stderr_streams_too_and_the_exit_code_rides_the_result() {
        let tools = ShellTools::new(std::env::temp_dir());
        tools.install_approval(Arc::new(AllowAll));
        let streamed = Arc::new(Mutex::new(Vec::new()));
        let collector = streamed.clone();
        let mut progress = OutputOnly(move |chunk: &str| {
            collector
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(chunk.to_owned());
        });
        let result = tools
            .resolve_streaming(
                call("echo oops >&2; exit 3"),
                &ToolContext::default(),
                &mut progress,
            )
            .await;

        assert_eq!(
            *streamed.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["oops\n".to_owned()]
        );
        assert!(result.is_error);
        assert_eq!(result.code, Some(3));
        assert!(result.content.contains("oops"));
        tools.clear_approval();
    }

    #[tokio::test]
    async fn a_cancelled_command_stops_mid_run() {
        let tools = ShellTools::new(std::env::temp_dir());
        tools.install_approval(Arc::new(AllowAll));
        let ctx = ToolContext::default();
        ctx.cancelled.store(true, Ordering::Relaxed);
        let result = tools.resolve(call("sleep 30"), &ctx).await;
        assert!(result.is_error);
        assert_eq!(result.content, "cancelled");
        tools.clear_approval();
    }

    #[test]
    fn the_installed_hook_reads_back_until_cleared() {
        struct Spy;
        impl Approval for Spy {
            fn approve(&self, _call: &ToolCall) -> bool {
                true
            }
        }

        let tools = ShellTools::new("/tmp");
        assert!(
            tools.current_approval().is_none(),
            "no hook installed: none readable"
        );
        let hook: Arc<dyn Approval> = Arc::new(Spy);
        tools.install_approval(hook.clone());
        let read = tools.current_approval().unwrap();
        // The same policy answers through the read handle.
        let probe = ToolCall {
            name: "bash".into(),
            ..ToolCall::default()
        };
        assert!(read.approve(&probe));
        tools.clear_approval();
        assert!(tools.current_approval().is_none());
    }
}
