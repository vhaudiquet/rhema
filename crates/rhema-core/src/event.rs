//! The streaming seam: events emitted during a step and the sink that
//! receives them.
//!
//! Ownership of events: the [`crate::ModelProvider`] emits streaming deltas
//! (`TextDelta`, `ToolCallStart`, `ToolCallArgsDelta`) while it streams; the
//! [`crate::Agent`] emits the lifecycle events (`StepStart`, `ToolResult`,
//! `StepEnd`). A provider that announces a tool call while streaming it is
//! not echoed by the engine — the engine only emits `ToolCallStart` for
//! calls the provider did not announce.

/// One streaming or lifecycle event of a step.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    /// The step begins; `iteration` counts the steps this agent has already
    /// completed.
    StepStart {
        /// Zero-based index of this step within the agent.
        iteration: u32,
    },
    /// A piece of assistant text, in stream order.
    TextDelta(&'a str),
    /// A tool call begins streaming; args follow as deltas.
    ToolCallStart {
        /// The call's correlation id.
        id: &'a str,
        /// The tool's name.
        name: &'a str,
    },
    /// A piece of a tool call's JSON arguments, in stream order.
    ToolCallArgsDelta(&'a str),
    /// A tool result was appended to the session history.
    ToolResult {
        /// The answered call's id.
        id: &'a str,
        /// Result payload.
        content: &'a str,
        /// Whether the call failed.
        is_error: bool,
        /// The process exit code of command-like tools; `None` otherwise.
        code: Option<i32>,
    },
    /// The step ends.
    StepEnd {
        /// Token accounting for the model call.
        usage: crate::output::Usage,
        /// Why the model stopped.
        stop_reason: &'a crate::output::StopReason,
    },
}

/// Receives a step's events. Drivers implement this to stream into a UI, a
/// log, or a channel; a simple collector is a `Vec` behind a closure.
///
/// `Send` because the sink reference crosses await points inside provider
/// implementations, which must produce `Send` futures.
pub trait EventSink: Send {
    /// Handle one event. Called synchronously, in stream order.
    fn emit(&mut self, event: Event<'_>);
}
