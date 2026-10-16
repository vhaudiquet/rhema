//! The capability seam: one primitive, not a registry.
//!
//! A [`ToolResolver`] decides which tools the model sees this step
//! ([`ToolResolver::specs`]) and turns a model's call into an in-band
//! [`ToolResult`]. Errors never abort a step — they become `is_error` results
//! the model can react to; an unknown tool name is the resolver's error
//! result, not a panic. Each tool also declares its [`ReplaySafety`]:
//! whether a committed call that lost its result to a crash may be
//! re-executed on resume or must be reported interrupted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolCall {
    /// Model-chosen correlation id; the result must carry it back.
    pub id: String,
    /// Name of the tool to invoke. Must be one of the offered specs.
    pub name: String,
    /// Arguments as JSON, validated against the spec's `parameters` schema.
    pub args: serde_json::Value,
    /// Opaque provider metadata that must round-trip with this call through
    /// the session history. Stateful providers mark their function calls
    /// (e.g. Gemini's thought signatures) and reject replays that lose the
    /// marker; the provider stores it here on the way in and reads it back
    /// on the way out. Crate 1 never interprets it — the provider owns the
    /// shape.
    #[serde(default)]
    pub provider_data: serde_json::Value,
}

/// The outcome of resolving one [`ToolCall`], appended to the session
/// history as a `Tool`-role message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The [`ToolCall::id`] this result answers.
    pub id: String,
    /// Result payload as text (JSON-serialized data, or an error message).
    pub content: String,
    /// Whether the call failed; the model reacts to this in-band.
    pub is_error: bool,
    /// The process exit code of command-like tools; `None` for everything
    /// else. UI-facing metadata — the model reads the status from
    /// `content`.
    #[serde(default)]
    pub code: Option<i32>,
}

impl ToolResult {
    /// A successful result for `id`.
    pub fn success(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error: false,
            code: None,
        }
    }

    /// A failed result for `id`; `content` should say what went wrong.
    pub fn failure(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error: true,
            code: None,
        }
    }

    /// Attaches the process exit code, for command results.
    pub fn with_code(mut self, code: i32) -> Self {
        self.code = Some(code);
        self
    }
}

/// Per-run tool execution environment, created by the driver so that
/// permissions, cwd, and cancellation are fixed before the step starts.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Working directory tool executions run in.
    pub cwd: PathBuf,
    /// Driver-chosen identifier of the current run, for tracing.
    pub run_id: String,
    /// Set to `true` by the driver to ask in-flight tools to stop; polling
    /// the flag is the resolver's responsibility.
    pub cancelled: Arc<AtomicBool>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            run_id: String::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Whether re-running a tool after an interruption can repeat an external
/// side effect. Declared per tool via [`ToolResolver::replay_safety`] and
/// consulted when a session resumes with a committed call whose result
/// never landed: a `Safe` call may be re-executed to finish the round, an
/// `Unsafe` one is reported interrupted so the model can verify state
/// before retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReplaySafety {
    /// Re-running may repeat an external effect — a post, a merge, a
    /// delete. The default: an unclassified tool is treated as
    /// side-effecting.
    #[default]
    Unsafe,
    /// Re-running is safe — read-only queries, pure computation. A resume
    /// may re-execute the call without asking the model again.
    Safe,
}

/// Turns a model's tool calls into results.
#[async_trait]
pub trait ToolResolver: Send + Sync {
    /// The tools offered to the model this step.
    fn specs(&self) -> Vec<crate::request::ToolSpec>;

    /// The replay safety of one offered tool (see [`ReplaySafety`]),
    /// consulted when a crash left a committed call without its result.
    /// Defaults to [`ReplaySafety::Unsafe`]: a resolver that does not
    /// classify its tools is treated as entirely side-effecting.
    fn replay_safety(&self, _name: &str) -> ReplaySafety {
        ReplaySafety::Unsafe
    }

    /// Turn a model's call into a result. Errors become in-band results:
    /// implementations must not panic and must not abort on unknown names
    /// or bad arguments — return [`ToolResult::failure`] instead.
    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult;

    /// Resolve with progress: an implementation running something long —
    /// a shell command — calls `progress` with human-facing output
    /// chunks (complete lines) as they land, so a sink can render them
    /// live; the full text still arrives in the returned result. The
    /// default ignores progress and resolves as usual.
    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        let _ = progress;
        self.resolve(call, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_without_a_code_field_loads_from_an_old_history() {
        let raw = r#"{"id":"c1","content":"ok","is_error":false}"#;
        let result: ToolResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.code, None);
        assert_eq!(result.with_code(3).code, Some(3));
    }
}
