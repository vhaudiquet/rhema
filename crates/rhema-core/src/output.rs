//! What comes back from the model: the assembled output and bookkeeping.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn on its own.
    EndTurn,
    /// The model requested tool calls; the engine resolves them in-step.
    ToolUse,
    /// Generation hit the token ceiling.
    MaxTokens,
    /// Generation hit a configured stop sequence.
    StopSequence,
    /// Anything provider-specific.
    Other(String),
}

/// Token accounting for one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens billed.
    pub input_tokens: u32,
    /// Completion tokens billed.
    pub output_tokens: u32,
    /// Subset of the prompt served from the provider's cache.
    pub cached_input_tokens: u32,
}

/// The complete result of one model call, already assembled from the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelOutput {
    /// The assistant message; the engine appends it to the session history.
    pub message: Message,
    /// Why generation stopped.
    pub stop_reason: StopReason,
    /// Token accounting.
    pub usage: Usage,
}

/// The result of one [`crate::Agent::step`]: the model output plus any
/// tool results the engine already appended to the session history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// The model output this step produced.
    pub output: ModelOutput,
}
