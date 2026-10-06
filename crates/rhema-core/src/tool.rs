//! The capability seam: one primitive, not a registry.
//!
//! A [`ToolResolver`] decides which tools the model sees this step
//! ([`ToolResolver::specs`]) and turns a model's call into an in-band
//! [`ToolResult`]. Errors never abort a step — they become `is_error` results
//! the model can react to; an unknown tool name is the resolver's error
//! result, not a panic.

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
}

impl ToolResult {
    /// A successful result for `id`.
    pub fn success(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result for `id`; `content` should say what went wrong.
    pub fn failure(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error: true,
        }
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

/// Turns a model's tool calls into results.
#[async_trait]
pub trait ToolResolver: Send + Sync {
    /// The tools offered to the model this step.
    fn specs(&self) -> Vec<crate::request::ToolSpec>;

    /// Turn a model's call into a result. Errors become in-band results:
    /// implementations must not panic and must not abort on unknown names
    /// or bad arguments — return [`ToolResult::failure`] instead.
    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult;
}
