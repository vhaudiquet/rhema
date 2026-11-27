//! What comes back from the model: the assembled output and bookkeeping.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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

/// Token and billing accounting for one model call.
#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens billed.
    pub input_tokens: u32,
    /// Completion tokens billed.
    pub output_tokens: u32,
    /// Subset of the prompt served from the provider's cache.
    pub cached_input_tokens: u32,
    /// What the provider says this call cost, in US dollars. A route
    /// that reports its own bill writes it here, so a frontend can
    /// charge the session the amount the provider charged instead of
    /// re-deriving it from a price list. `Some` is authoritative even
    /// when it is zero, a free model; `None` means the provider did
    /// not report a cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl std::fmt::Debug for Usage {
    /// Prints the token fields always, and the cost only when one was
    /// reported. The conditional field keeps the common `None` case
    /// exactly as it read before the cost existed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Usage");
        debug
            .field("input_tokens", &self.input_tokens)
            .field("output_tokens", &self.output_tokens)
            .field("cached_input_tokens", &self.cached_input_tokens);
        if let Some(cost) = self.cost_usd {
            debug.field("cost_usd", &cost);
        }
        debug.finish()
    }
}

impl Usage {
    /// Adds one call's reported cost to a cumulative tally. A `None`
    /// keeps an existing figure unchanged, so a provider that reports
    /// its bill on one frame and not another does not erase it.
    pub fn add_cost(&mut self, cost: Option<f64>) {
        if let Some(cost) = cost {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost);
        }
    }
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

impl Step {
    /// Whether this step ran any tool call. A provider can stop on
    /// [`StopReason::ToolUse`] without assembling a call, a `[DONE]`
    /// with no fragment, a dropped stream; such a step has nothing for
    /// another request to answer, and the turn loops check this so they
    /// do not spend a request per iteration on it.
    pub fn ran_tools(&self) -> bool {
        self.output
            .message
            .content
            .iter()
            .any(|content| matches!(content, crate::message::Content::ToolCall(_)))
    }
}
