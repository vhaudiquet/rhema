//! Anthropic wire types and the pure mapping between them and rhema-core's
//! model. No I/O here — [`crate::provider`] drives these functions.
//!
//! Mapping constraints (the Messages API):
//! - `max_tokens` is required on every call; when
//!   [`rhema_core::ChatParams::max_tokens`] is unset,
//!   [`DEFAULT_MAX_TOKENS`] is sent.
//! - `ModelRequest.system` and the text of every `Role::System` message
//!   fold into the top-level `system` string, joined by a blank line — the
//!   API has no system role inside `messages`.
//! - `Role::User` and `Role::Assistant` map to their like-named roles.
//!   `Role::Tool` has no wire role: its results become `tool_result`
//!   blocks, which the API only accepts inside `user` messages. Consecutive
//!   user-role turns (several `Tool` messages, or a `Tool` message followed
//!   by a `User` one) merge into one message, and `tool_result` blocks are
//!   hoisted ahead of text blocks — the API expects results at the top of
//!   the user turn.
//! - `tool_use.input` must be a JSON object: objects pass through, absent
//!   args become `{}`, anything else is wrapped under [`WRAPPED_ARGS_KEY`].
//!   Call ids and names round-trip verbatim.
//! - Reasoning replay: a streamed thinking block never joins the assembled
//!   message, but it is the chain of thought that led to the next call, so
//!   [`ChunkAssembler::finish`] stores its text and signature on that
//!   call's `ToolCall::provider_data` — only with a signature, which the
//!   API requires to accept a thinking block back — and [`request_body`]
//!   prepends the matching `thinking` block ahead of the call, but only
//!   while thinking stays enabled: the API rejects thinking blocks on
//!   plain turns.
//! - `tools[]` entries are `{name, description, input_schema}`;
//!   `input_schema` is required and must be an object schema, so a
//!   non-object spec degrades to the empty schema rather than dropping the
//!   tool.
//! - Cache breakpoints (the prompt cache's only lookup positions) mark
//!   four positions, the API's maximum: the system block, the last
//!   `tools[]` entry, and the last blocks of the last two user turns.
//!   The message pair rolls forward one turn per request, so each
//!   request marks where the previous one ended and the conversation
//!   prefix reads from the cache instead of re-billing at the write
//!   rate.
//! - Empty text blocks are dropped: the API rejects them.
//! - Every call streams (`stream: true`). The SSE event sequence folds in
//!   arrival order: `content_block_start` announces text and `tool_use`
//!   blocks (a `tool_use` emits `ToolCallStart` with the wire id, or a
//!   synthesized `call_{index}` when absent), `content_block_delta` streams
//!   `text_delta` pieces as `TextDelta` and `input_json_delta` pieces as
//!   `ToolCallArgsDelta`, and `message_delta` carries the final stop
//!   reason. `ping`, `content_block_stop`, `message_stop`, and unknown
//!   event kinds carry no rhema-mappable state; `thinking_delta` pieces
//!   stream as [`Event::ThinkingDelta`] and never reach the assembled
//!   message, whose `signature_delta` pieces feed the block's replay
//!   signature instead; a top-level `error` event fails the stream.
//! - Usage merges per field: `message_start`'s nested `usage` carries the
//!   input-token count, `message_delta`'s the final output-token count.
//!   The totals normalize to the inclusive convention the other adapters
//!   use: `input_tokens` counts the whole prompt — the uncached input
//!   plus both cache subsets (writes bill near the full input price,
//!   reads are the cached subset) — and `cache_read_input_tokens` maps
//!   to cached tokens (0 when absent). A prompt served whole from cache
//!   reports its whole size, never just the new tokens.
//! - Stop reasons: `end_turn` -> EndTurn, `tool_use` -> ToolUse,
//!   `max_tokens` -> MaxTokens, `stop_sequence` -> StopSequence, anything
//!   else -> Other. Anthropic's reason is authoritative — `tool_use` rides
//!   the wire exactly when tool blocks were streamed — so, unlike the
//!   sibling adapters, the assembled message never overrides it.
//! - Models: `GET /v1/models` paginates with `has_more` + `after_id`
//!   (cursor `last_id`). The list carries only id and display name;
//!   description, ceilings, and pricing have no wire source and default
//!   empty.
//! - Quota: the GLM Coding Plan's monitor endpoint
//!   (`GET /api/monitor/usage/quota/limit`, Z.ai hosts only) reports each
//!   metered window as a `percentage` spent; the normalized allowance is
//!   what is left of the 5-hour and weekly windows, the tool quota left
//!   out.

use std::collections::BTreeMap;

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::{ModelRequest, ThinkingEffort};
use rhema_core::tool::ToolCall;
use rhema_provider::{Allowance, ModelCard};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker key wrapping non-object `tool_use.input` into a valid object.
const WRAPPED_ARGS_KEY: &str = "_rhema_args";

/// Marker key wrapping an unparseable streamed arguments payload so the
/// call still round-trips through history.
const WRAPPED_ARGS_TEXT_KEY: &str = "_anthropic_args_text";

/// The `max_tokens` sent when [`rhema_core::ChatParams::max_tokens`] is
/// unset: the field is required by the API, and this ceiling is accepted
/// by every current model — Anthropic's own and the Z.ai plan route —
/// while sitting high enough that a long reply or a large generated file
/// is not cut off mid-stream (a `max_tokens` stop ends the turn with the
/// answer truncated).
pub const DEFAULT_MAX_TOKENS: u32 = 32_768;

// --- Request wire types (serialize) ---

/// The `POST /v1/messages` request body. Always streaming; usage needs no
/// opt-in on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessagesRequest {
    /// Model identifier, passed through verbatim.
    pub model: String,
    /// Output token ceiling; the API rejects calls without one.
    pub max_tokens: u32,
    /// The folded system prompt, when the request has one — as a block
    /// array so its end can carry a cache breakpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<WireSystemBlock>>,
    /// Conversation turns; roles are only `user` and `assistant`.
    pub messages: Vec<WireMessage>,
    /// Tools offered this step, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    /// Sampling temperature, when set — and never set while thinking is
    /// enabled: the API rejects temperature modifications on thinking
    /// turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Extended thinking, when the driver named a level: the API's knob
    /// is a token budget, not a level, so the level maps onto one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<WireThinking>,
    /// Always `true`: the provider only speaks the streaming dialect.
    pub stream: bool,
}

/// The `thinking` request field: extended thinking with a token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WireThinking {
    /// Always `"enabled"`; there is no disabled form — absence is.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The reasoning token budget.
    pub budget_tokens: u32,
}

/// The reasoning budget a thinking level maps onto.
fn thinking_budget(thinking: ThinkingEffort) -> Option<u32> {
    match thinking {
        ThinkingEffort::Default => None,
        ThinkingEffort::Low => Some(4_096),
        ThinkingEffort::Medium => Some(16_384),
        ThinkingEffort::High => Some(32_768),
    }
}

/// One turn on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessage {
    /// `"user"` or `"assistant"`.
    pub role: String,
    /// Ordered content blocks of the turn.
    pub content: Vec<WireBlock>,
}

/// One content block of a [`WireMessage`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireBlock {
    /// Model or user text.
    Text {
        /// The text itself; never empty.
        text: String,
        /// The cache breakpoint marker, when this block ends a cached
        /// prefix.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<WireCacheControl>,
    },
    /// The model's reasoning replayed from history (assistant turns): an
    /// interleaved-thinking model continues its chain of thought across
    /// tool calls instead of re-deriving it.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// The signature the API issued with the trace; it rejects the
        /// block without one.
        signature: String,
    },
    /// A tool invocation requested by the model (assistant turns).
    ToolUse {
        /// The call's correlation id, echoed from history.
        id: String,
        /// Name of the called tool.
        name: String,
        /// Call arguments; must be a JSON object on the wire.
        input: Value,
    },
    /// A result answering a tool call (user turns only).
    ToolResult {
        /// The answered call's id.
        tool_use_id: String,
        /// Result payload as text.
        content: String,
        /// Whether the call failed; the model reacts to this in-band.
        is_error: bool,
        /// The cache breakpoint marker, when this block ends a cached
        /// prefix.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<WireCacheControl>,
    },
}

impl WireBlock {
    /// Marks this block as a cache breakpoint. Assistant blocks are
    /// never the last block of a request — a turn always ends with the
    /// user's input or a tool result — so only the user-side variants
    /// carry the marker.
    fn mark_ephemeral(&mut self) {
        match self {
            WireBlock::Text { cache_control, .. } | WireBlock::ToolResult { cache_control, .. } => {
                *cache_control = EPHEMERAL;
            }
            _ => {}
        }
    }
}

/// A `tools[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool {
    /// Name the model uses to call the tool.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The tool's argument schema as full JSON Schema.
    pub input_schema: Value,
    /// The cache breakpoint marker, when this tool ends the cached tool
    /// segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<WireCacheControl>,
}

/// The `cache_control` block annotation: marks a prefix segment for
/// Anthropic's prompt cache. `ephemeral` is the only kind the API offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WireCacheControl {
    /// Always `"ephemeral"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// The cache breakpoint every marked segment carries.
pub const EPHEMERAL: Option<WireCacheControl> = Some(WireCacheControl { kind: "ephemeral" });

/// One block of the `system` prompt array.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireSystemBlock {
    /// Always `"text"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The prompt text.
    pub text: String,
    /// The cache breakpoint marker, when this block ends a cached prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<WireCacheControl>,
}

// --- Response wire types (deserialize) ---

/// One SSE stream event of a Messages API response. Every event carries its
/// `type` in the payload, so the flat shape covers them all; fields absent
/// on a kind deserialize to `None`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct StreamEvent {
    /// The event's `type` (`message_start`, `content_block_start`,
    /// `content_block_delta`, `content_block_stop`, `message_delta`,
    /// `message_stop`, `ping`, `error`).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The error detail, on an event aborted by a failure.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
    /// The message header, on `message_start`.
    #[serde(default)]
    pub message: Option<WireStartMessage>,
    /// Which content block the event is about, on `content_block_*`.
    #[serde(default)]
    pub index: Option<u32>,
    /// The block's opening state, on `content_block_start`.
    #[serde(default)]
    pub content_block: Option<WireContentBlock>,
    /// The piece generated since the last event, on `content_block_delta`
    /// and `message_delta`.
    #[serde(default)]
    pub delta: Option<WireDelta>,
    /// Token accounting, on `message_delta`.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// The `message` object of a `message_start` event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireStartMessage {
    /// The message's provider id.
    #[serde(default)]
    pub id: Option<String>,
    /// Prompt-to-first-token accounting.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// The `content_block` object of a `content_block_start` event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireContentBlock {
    /// The block's `type` (`text`, `tool_use`, `thinking`, ...).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The call's correlation id, on `tool_use` blocks.
    #[serde(default)]
    pub id: Option<String>,
    /// The tool's name, on `tool_use` blocks.
    #[serde(default)]
    pub name: Option<String>,
    /// The block's opening text; empty on `text` blocks.
    #[serde(default)]
    pub text: Option<String>,
}

/// The `delta` object of a `content_block_delta` or `message_delta` event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireDelta {
    /// The delta's `type` (`text_delta`, `input_json_delta`,
    /// `thinking_delta`, `signature_delta`); absent on `message_delta`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// A piece of assistant text, on `text_delta`.
    #[serde(default)]
    pub text: Option<String>,
    /// A piece of the model's reasoning trace, on `thinking_delta`.
    /// Emitted as [`Event::ThinkingDelta`], never assembled into the
    /// message.
    #[serde(default)]
    pub thinking: Option<String>,
    /// The trace's replay signature, on `signature_delta`: the API only
    /// accepts a thinking block back with the signature it was issued.
    #[serde(default)]
    pub signature: Option<String>,
    /// A piece of a tool call's JSON arguments, on `input_json_delta`.
    #[serde(default)]
    pub partial_json: Option<String>,
    /// Why generation stopped, on `message_delta`.
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// A wire usage object. Each frame carries only its own accounting: the
/// `message_start` one the input count, the `message_delta` one the final
/// output count; either may add the cache numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireUsage {
    /// Prompt tokens billed.
    #[serde(default)]
    pub input_tokens: Option<u32>,
    /// Completion tokens billed.
    #[serde(default)]
    pub output_tokens: Option<u32>,
    /// Prompt subset served from Anthropic's prompt cache.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    /// Prompt subset written to the cache this request (billed at the
    /// write rate, near the full input price).
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
}

/// The `error` object of an `error` event, or of a non-2xx response body:
/// `{"type": "error", "error": {"type": ..., "message": ...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireErrorBody {
    /// The error detail, when the body has the documented shape.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// Anthropic error detail: `{"type": "overloaded_error", "message": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireErrorDetail {
    /// The API's error type (e.g. `overloaded_error`).
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Human-readable description.
    #[serde(default)]
    pub message: Option<String>,
}

impl WireErrorBody {
    /// The body's best human-readable message, falling back to the raw body
    /// when it does not have the documented shape.
    pub fn message(&self, raw: &str) -> String {
        self.error
            .as_ref()
            .and_then(|e| e.message.clone())
            .unwrap_or_else(|| raw.to_owned())
    }
}

// --- ListModels: wire -> normalized ---

/// One page of the `GET /v1/models` response.
#[derive(Debug, Default, Deserialize)]
pub struct WireModelsPage {
    /// The models on this page.
    #[serde(default)]
    pub data: Vec<WireModel>,
    /// Whether another page follows.
    #[serde(default)]
    pub has_more: bool,
    /// The last row's id, the cursor for the next page's `after_id`.
    #[serde(default)]
    pub last_id: Option<String>,
}

/// One wire model object; the list endpoint carries nothing beyond
/// identity, so the row normalizes almost entirely to defaults.
#[derive(Debug, Default, Deserialize)]
pub struct WireModel {
    /// Model id (e.g. `claude-sonnet-4-5`), usable as the request's
    /// `model` value.
    #[serde(default)]
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub display_name: String,
}

impl From<WireModel> for ModelCard {
    fn from(wire: WireModel) -> Self {
        Self {
            id: wire.id,
            display_name: wire.display_name,
            // The models list carries none of the catalog fields the
            // normalized row shows: no description, no context or output
            // ceilings, no pricing.
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            // Every listed model is offered through the Messages API.
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }
}

// --- Z.ai quota: the Coding Plan monitor endpoint -> normalized ---

/// The `GET /api/monitor/usage/quota/limit` response of Z.ai's GLM
/// Coding Plan — an undocumented monitor endpoint (the plan's web
/// console builds on it) served beside the Anthropic route on the Z.ai
/// hosts. A rejected token answers with HTTP 200 and `success` false.
#[derive(Debug, Default, Deserialize)]
pub struct WireZaiQuota {
    /// Whether the call succeeded.
    #[serde(default)]
    pub success: bool,
    /// The numeric code of a failed call (`401` for a rejected token).
    #[serde(default)]
    pub code: Option<u32>,
    /// The error text of a failed call.
    #[serde(default)]
    pub msg: Option<String>,
    /// The quota report; `None` when the body is an error envelope.
    #[serde(default)]
    pub data: Option<WireZaiQuotaData>,
}

/// The `data` object of a [`WireZaiQuota`].
#[derive(Debug, Default, Deserialize)]
pub struct WireZaiQuotaData {
    /// The plan tier (`LITE`, `PRO`), when reported.
    #[serde(default)]
    pub level: Option<String>,
    /// One entry per metered window and per tool allowance.
    #[serde(default)]
    pub limits: Vec<WireZaiLimit>,
}

/// One entry of a [`WireZaiQuotaData`]'s `limits`.
#[derive(Debug, Default, Deserialize)]
pub struct WireZaiLimit {
    /// What the entry meters: `TOKENS_LIMIT` on token-metered plans
    /// (`CREDIT_LIMIT` on credit-metered ones) is a coding window;
    /// `TIME_LIMIT` is the monthly MCP tool-call quota, which the coding
    /// session never gates on.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// The window's length code: 3 is the 5-hour rolling window, 6 the
    /// weekly one, 5 the monthly tool allowance.
    #[serde(default)]
    pub unit: Option<u32>,
    /// The share of the window's budget already spent, 0–100.
    #[serde(default)]
    pub percentage: Option<f64>,
}

/// The plan allowance the quota report describes: what is left of each
/// coding window, the 5-hour one first — `"5h 53% wk 88%"` for the
/// footer's badge, a space between the windows because they spend one
/// pool (the footer's `·` marks a new fact, not a detail of one). The
/// tool quota stays out. `None` when the report
/// names neither window: a shape drift maps to no badge, never to a
/// wrong figure.
pub fn allowance(quota: &WireZaiQuota) -> Option<Allowance> {
    let data = quota.data.as_ref()?;
    let mut windows = [None, None]; // The left share of 5-hour, weekly.
    for limit in &data.limits {
        if !matches!(
            limit.kind.as_deref(),
            Some("TOKENS_LIMIT") | Some("CREDIT_LIMIT")
        ) {
            continue;
        }
        let Some(percentage) = limit.percentage else {
            continue;
        };
        let left = (100.0 - percentage).clamp(0.0, 100.0);
        match limit.unit {
            Some(3) => windows[0] = Some(left),
            Some(6) => windows[1] = Some(left),
            _ => {}
        }
    }
    let mut amount = String::new();
    for (left, window) in windows.iter().zip(["5h", "wk"]) {
        let Some(left) = left else { continue };
        if !amount.is_empty() {
            amount.push(' ');
        }
        amount.push_str(&format!("{window} {left:.0}%"));
    }
    (!amount.is_empty()).then_some(Allowance {
        amount,
        kind: String::new(),
    })
}

// --- Mapping: ModelRequest -> wire ---

/// Builds the Messages request body for `req`.
///
/// # Errors
///
/// Fails with [`ProviderError::InvalidRequest`] when the model is unset or
/// the request would produce no messages (the API rejects empty `messages`).
pub fn request_body(req: &ModelRequest) -> Result<MessagesRequest, ProviderError> {
    if req.params.model.trim().is_empty() {
        return Err(ProviderError::InvalidRequest(
            "chat params have no model".into(),
        ));
    }

    let mut system_texts: Vec<String> = Vec::new();
    if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
        system_texts.push(system.to_owned());
    }
    // Extended thinking is decided before the turns map: the assistant
    // replay below must know whether the API will accept thinking blocks
    // back on this request.
    let thinking = thinking_budget(req.params.thinking).map(|budget| WireThinking {
        kind: "enabled",
        budget_tokens: budget,
    });
    let mut messages: Vec<WireMessage> = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::System => system_texts.extend(texts_of(message)),
            Role::Assistant => {
                // History replays keep content order: the model generated
                // text and calls in this order.
                let mut blocks: Vec<WireBlock> = Vec::new();
                for content in &message.content {
                    match content {
                        Content::Text(text) if !text.is_empty() => {
                            blocks.push(WireBlock::Text {
                                text: text.clone(),
                                cache_control: None,
                            });
                        }
                        Content::ToolCall(call) => {
                            // The trace that produced the call replays
                            // ahead of it: an interleaved-thinking model
                            // continues its chain of thought instead of
                            // re-deriving it. The API rejects thinking
                            // blocks on plain turns, so the replay rides
                            // only when a budget is set.
                            if thinking.is_some() {
                                if let Some((text, signature)) = call
                                    .provider_data
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .zip(
                                        call.provider_data.get("signature").and_then(Value::as_str),
                                    )
                                {
                                    blocks.push(WireBlock::Thinking {
                                        thinking: text.to_owned(),
                                        signature: signature.to_owned(),
                                    });
                                }
                            }
                            blocks.push(WireBlock::ToolUse {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                input: object_args(&call.args),
                            });
                        }
                        _ => {}
                    }
                }
                push_turn(&mut messages, "assistant", blocks);
            }
            Role::User | Role::Tool => {
                // Results ride in user turns, ahead of any text: the API
                // expects them at the top of the user turn that answers the
                // assistant's `tool_use` blocks.
                let mut blocks: Vec<WireBlock> = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolResult(result) => Some(WireBlock::ToolResult {
                            tool_use_id: result.id.clone(),
                            content: result.content.clone(),
                            is_error: result.is_error,
                            cache_control: None,
                        }),
                        _ => None,
                    })
                    .collect();
                blocks.extend(message.content.iter().filter_map(|content| match content {
                    Content::Text(text) if !text.is_empty() => Some(WireBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    }),
                    _ => None,
                }));
                push_turn(&mut messages, "user", blocks);
            }
        }
    }

    if messages.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "request has no messages to send".into(),
        ));
    }

    // The last two user turns each end a cached prefix. A cache lookup
    // happens only at a marked position, so this request's last-turn
    // marker is read back only if the next request still marks where
    // this one ends — the second-to-last user turn's marker is that
    // position, and the rolling pair turns each step's conversation
    // delta into a cache read instead of a full cache-write re-bill.
    // With the system and tools markers that is four breakpoints, the
    // API's maximum.
    let user_turns = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == "user")
        .map(|(index, _)| index);
    if let Some(previous) = user_turns.rev().nth(1) {
        if let Some(block) = messages[previous].content.last_mut() {
            block.mark_ephemeral();
        }
    }
    if let Some(last_message) = messages.last_mut() {
        if let Some(last_block) = last_message.content.last_mut() {
            last_block.mark_ephemeral();
        }
    }

    let tools = (!req.tools.is_empty()).then(|| {
        let count = req.tools.len();
        req.tools
            .iter()
            .enumerate()
            .map(|(index, spec)| WireTool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema: object_schema(&spec.parameters),
                // The last tool ends the cached tool segment: tools sit
                // at the head of every prompt, so this breakpoint hits
                // on every step.
                cache_control: (index + 1 == count).then_some(EPHEMERAL).flatten(),
            })
            .collect()
    });

    // The budget must sit below max_tokens, so a thinking request raises
    // the ceiling when the driver's own is too tight; and the API
    // rejects temperature modifications on thinking turns.
    let max_tokens = req
        .params
        .max_tokens
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .max(thinking.map_or(0, |t| t.budget_tokens + 4_096));

    Ok(MessagesRequest {
        model: req.params.model.clone(),
        max_tokens,
        system: (!system_texts.is_empty()).then(|| {
            vec![WireSystemBlock {
                kind: "text",
                text: system_texts.join("\n\n"),
                // The system prompt is byte-stable for a session's life:
                // this breakpoint hits on every step after the first.
                cache_control: EPHEMERAL,
            }]
        }),
        messages,
        tools,
        temperature: thinking
            .is_none()
            .then_some(req.params.temperature)
            .flatten(),
        thinking,
        stream: true,
    })
}

/// Appends one wire turn, merging it into the previous one when the roles
/// match: the API wants alternating `user`/`assistant` turns, and several
/// rhema turns map onto one wire role (e.g. a `Tool` message followed by a
/// `User` one).
fn push_turn(messages: &mut Vec<WireMessage>, role: &str, blocks: Vec<WireBlock>) {
    if blocks.is_empty() {
        return;
    }
    if messages.last().is_some_and(|last| last.role == role) {
        messages
            .last_mut()
            .expect("checked above")
            .content
            .extend(blocks);
    } else {
        messages.push(WireMessage {
            role: role.to_owned(),
            content: blocks,
        });
    }
}

/// The text blocks of a message.
fn texts_of(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `tool_use.input` must be a JSON object: objects pass through, absent
/// args become `{}`, anything else is wrapped under [`WRAPPED_ARGS_KEY`].
fn object_args(args: &Value) -> Value {
    if args.is_object() {
        args.clone()
    } else if args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::json!({ WRAPPED_ARGS_KEY: args })
    }
}

/// `input_schema` is required and must be an object schema: objects pass
/// through verbatim, anything else degrades to the empty schema rather than
/// dropping the tool.
fn object_schema(parameters: &Value) -> Value {
    if parameters.is_object() {
        parameters.clone()
    } else {
        serde_json::json!({ "type": "object" })
    }
}

// --- Mapping: wire events -> ModelOutput / events ---

/// One streamed content block being assembled across deltas, keyed by its
/// stream index.
#[derive(Debug)]
enum PartialBlock {
    /// Model text, accumulated from `text_delta` pieces.
    Text {
        /// The accumulated text.
        text: String,
    },
    /// The model's reasoning trace, accumulated from `thinking_delta`
    /// pieces. Streams live and never joins the assembled message; the
    /// finished trace attaches to the next call for replay.
    Thinking {
        /// The accumulated reasoning text.
        text: String,
        /// The accumulated signature, from `signature_delta` pieces: the
        /// API only replays a thinking block it signed.
        signature: String,
    },
    /// A tool call: id and name from `content_block_start`, arguments from
    /// `input_json_delta` pieces.
    ToolCall {
        /// The wire id, when the block carried one.
        id: Option<String>,
        /// The tool's name.
        name: String,
        /// The accumulated arguments JSON text.
        json: String,
    },
}

/// Assembles SSE stream events into the final [`ModelOutput`], emitting
/// streaming events as deltas arrive.
///
/// Block indexes arrive in stream order, so the keyed map preserves the
/// wire's block order in the assembled message. Usage is per-field
/// last-wins: each frame carries only its own accounting.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    blocks: BTreeMap<u32, PartialBlock>,
    stop_reason: Option<String>,
    usage: Usage,
}

impl ChunkAssembler {
    /// An assembler that has seen no events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one stream event, emitting its streaming events.
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Api`] (no status) when the event is
    /// the stream's `error` object.
    pub fn push(
        &mut self,
        event: StreamEvent,
        sink: &mut dyn EventSink,
    ) -> Result<(), ProviderError> {
        // The event type is authoritative: even a bare error frame with no
        // detail must fail the stream.
        if event.kind == "error" {
            return Err(ProviderError::Api {
                message: event
                    .error
                    .and_then(|error| error.message)
                    .unwrap_or_else(|| "mid-stream error".to_owned()),
                status: None,
            });
        }
        // `message_start` nests its usage under the message header,
        // `message_delta` carries it top-level.
        let mut usage_frame = false;
        if let Some(usage) = event
            .message
            .as_ref()
            .and_then(|message| message.usage.as_ref())
        {
            merge_usage(&mut self.usage, usage);
            usage_frame = true;
        }
        if let Some(usage) = &event.usage {
            merge_usage(&mut self.usage, usage);
            usage_frame = true;
        }
        // The tally streams live: the prompt lands with `message_start`,
        // cumulative completions ride any interim `message_delta`.
        if usage_frame {
            sink.emit(Event::Usage(self.usage));
        }
        match event.kind.as_str() {
            "content_block_start" => {
                let index = event.index.unwrap_or_default();
                let Some(block) = &event.content_block else {
                    return Ok(());
                };
                match block.kind.as_str() {
                    "tool_use" => {
                        let id = block
                            .id
                            .clone()
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| format!("call_{index}"));
                        let name = block.name.clone().unwrap_or_default();
                        sink.emit(Event::ToolCallStart {
                            id: &id,
                            name: &name,
                        });
                        self.blocks.insert(
                            index,
                            PartialBlock::ToolCall {
                                id: Some(id),
                                name,
                                json: String::new(),
                            },
                        );
                    }
                    "text" => {
                        self.blocks.insert(
                            index,
                            PartialBlock::Text {
                                text: String::new(),
                            },
                        );
                    }
                    "thinking" => {
                        self.blocks.insert(
                            index,
                            PartialBlock::Thinking {
                                text: String::new(),
                                signature: String::new(),
                            },
                        );
                    }
                    // Future block kinds carry no rhema-mappable content.
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = event.index.unwrap_or_default();
                let Some(delta) = &event.delta else {
                    return Ok(());
                };
                match delta.kind.as_deref() {
                    Some("text_delta") => {
                        let Some(text) = delta.text.as_deref().filter(|text| !text.is_empty())
                        else {
                            return Ok(());
                        };
                        sink.emit(Event::TextDelta(text));
                        match self.blocks.entry(index).or_insert(PartialBlock::Text {
                            text: String::new(),
                        }) {
                            PartialBlock::Text { text: accumulated } => accumulated.push_str(text),
                            // A delta for a block the start event declared a
                            // call or a trace stays that kind; the stray text
                            // has no home.
                            PartialBlock::ToolCall { .. } | PartialBlock::Thinking { .. } => {}
                        }
                    }
                    Some("input_json_delta") => {
                        let Some(piece) = delta
                            .partial_json
                            .as_deref()
                            .filter(|piece| !piece.is_empty())
                        else {
                            return Ok(());
                        };
                        // The id comes from the block's start event; a
                        // stray delta with none behind it synthesizes the
                        // id that start would have used.
                        let id = match self.blocks.get(&index) {
                            Some(PartialBlock::ToolCall { id, .. }) => {
                                id.clone().unwrap_or_else(|| format!("call_{index}"))
                            }
                            _ => format!("call_{index}"),
                        };
                        sink.emit(Event::ToolCallArgsDelta {
                            id: &id,
                            delta: piece,
                        });
                        match self.blocks.entry(index).or_insert(PartialBlock::ToolCall {
                            id: None,
                            name: String::new(),
                            json: String::new(),
                        }) {
                            PartialBlock::ToolCall { json, .. } => json.push_str(piece),
                            // A delta for a block the start event declared
                            // text or a trace stays that kind.
                            PartialBlock::Text { .. } | PartialBlock::Thinking { .. } => {}
                        }
                    }
                    Some("thinking_delta") => {
                        let Some(text) =
                            delta.thinking.as_deref().filter(|piece| !piece.is_empty())
                        else {
                            return Ok(());
                        };
                        sink.emit(Event::ThinkingDelta(text));
                        // A delta for a block the start event declared
                        // another kind stays that kind; only a trace grows.
                        if let PartialBlock::Thinking {
                            text: accumulated, ..
                        } = self.blocks.entry(index).or_insert(PartialBlock::Thinking {
                            text: String::new(),
                            signature: String::new(),
                        }) {
                            accumulated.push_str(text);
                        }
                    }
                    Some("signature_delta") => {
                        let Some(signature) =
                            delta.signature.as_deref().filter(|piece| !piece.is_empty())
                        else {
                            return Ok(());
                        };
                        // A delta for a block the start event declared
                        // another kind stays that kind; only a trace grows
                        // a signature.
                        if let PartialBlock::Thinking {
                            signature: accumulated,
                            ..
                        } = self.blocks.entry(index).or_insert(PartialBlock::Thinking {
                            text: String::new(),
                            signature: String::new(),
                        }) {
                            accumulated.push_str(signature);
                        }
                    }
                    // Future kinds carry no rhema-mappable content.
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event.delta.and_then(|delta| delta.stop_reason) {
                    self.stop_reason = Some(reason);
                }
            }
            // Pings, block/message stops, and future event kinds carry no
            // rhema-mappable state.
            _ => {}
        }
        Ok(())
    }

    /// Consumes the assembler into the final output. The message is always
    /// `Role::Assistant`: blocks in stream order, empty text blocks
    /// dropped; an empty stream yields an empty content list, not an error.
    pub fn finish(self) -> ModelOutput {
        let mut content: Vec<Content> = Vec::new();
        // The trace streamed since the last call: the chain of thought that
        // led to it, replayed on that call. Text never consumes it, and a
        // call spends it — replay or not — so a later call cannot inherit
        // it.
        let mut pending: Option<(String, String)> = None;
        for (index, block) in self.blocks {
            match block {
                PartialBlock::Text { text } if !text.is_empty() => {
                    content.push(Content::Text(text));
                }
                PartialBlock::Text { .. } => {}
                PartialBlock::Thinking { text, signature } if !text.is_empty() => {
                    pending = Some((text, signature));
                }
                PartialBlock::Thinking { .. } => {}
                PartialBlock::ToolCall { id, name, json } => {
                    // The API rejects an unsigned thinking block, so a
                    // signature-less trace replays as nothing.
                    let provider_data = pending
                        .take()
                        .filter(|(_, signature)| !signature.is_empty())
                        .map(|(text, signature)| {
                            serde_json::json!({ "thinking": text, "signature": signature })
                        })
                        .unwrap_or(Value::Null);
                    content.push(Content::ToolCall(ToolCall {
                        id: id.unwrap_or_else(|| format!("call_{index}")),
                        name,
                        args: parse_args(&json),
                        provider_data,
                    }));
                }
            }
        }
        ModelOutput {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: stop_reason(self.stop_reason.as_deref()),
            usage: self.usage,
        }
    }
}

/// Merges one wire usage frame into the running tally; absent fields keep
/// the value a previous frame set.
fn merge_usage(usage: &mut Usage, wire: &WireUsage) {
    // The input total is inclusive: the API's own input_tokens count
    // excludes both cache subsets, so both fold in — the writes (which
    // bill near the full input price) and the reads (which the cached
    // subset carries, disjoint from the rest). That keeps every
    // downstream consumer — the context footprint and the cost math —
    // on the inclusive convention the other adapters use: input is the
    // whole prompt, cached the part of it served from cache. A prompt
    // fully served from cache reports its whole size, never just the
    // new tokens.
    let input = match (
        wire.input_tokens,
        wire.cache_creation_input_tokens,
        wire.cache_read_input_tokens,
    ) {
        (None, None, None) => None,
        (input, writes, reads) => {
            Some(input.unwrap_or(0) + writes.unwrap_or(0) + reads.unwrap_or(0))
        }
    };
    if let Some(input) = input {
        usage.input_tokens = input;
    }
    if let Some(output) = wire.output_tokens {
        usage.output_tokens = output;
    }
    if let Some(cached) = wire.cache_read_input_tokens {
        usage.cached_input_tokens = cached;
    }
}

/// Parses a block's accumulated `input_json_delta` text; an empty payload
/// is `{}` and an unparseable one wraps under [`WRAPPED_ARGS_TEXT_KEY`] so
/// the turn survives.
fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({ WRAPPED_ARGS_TEXT_KEY: raw }))
}

/// Maps the wire `stop_reason` onto rhema-core's stop reason. Anthropic's
/// reason is authoritative — `tool_use` rides the wire exactly when tool
/// blocks were streamed — so the assembled message never overrides it.
fn stop_reason(stop_reason: Option<&str>) -> StopReason {
    match stop_reason {
        None | Some("") | Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::event::Event;
    use rhema_core::request::{ChatParams, ToolSpec};
    use rhema_core::tool::ToolResult;
    use serde_json::json;

    /// Records events as their debug strings; `Event` borrows, so this keeps
    /// the assertions format-based.
    struct Collector(Vec<String>);

    impl Collector {
        fn new() -> Self {
            Self(Vec::new())
        }
    }

    impl EventSink for Collector {
        fn emit(&mut self, event: Event<'_>) {
            self.0.push(format!("{event:?}"));
        }
    }

    fn event(json: &str) -> StreamEvent {
        serde_json::from_str(json).expect("event fixture parses")
    }

    #[test]
    fn a_named_thinking_effort_maps_to_a_budget() {
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: "claude-sonnet-4-5".into(),
                temperature: Some(0.7),
                max_tokens: Some(4_096),
                thinking: ThinkingEffort::High,
            },
        };
        let body = serde_json::to_value(request_body(&req).unwrap()).expect("body serializes");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 32_768);
        // The ceiling rises above the budget, and temperature is dropped:
        // the API rejects temperature modifications on thinking turns.
        assert!(
            body["max_tokens"]
                .as_u64()
                .is_some_and(|ceiling| ceiling >= 32_768 + 4_096)
        );
        assert!(body.get("temperature").is_none());

        // Default sends no thinking object and keeps the temperature.
        req.params.thinking = ThinkingEffort::Default;
        let body = serde_json::to_value(request_body(&req).unwrap()).expect("body serializes");
        assert!(body.get("thinking").is_none(), "{body}");
        assert_eq!(body["temperature"].as_f64(), Some(0.7f32 as f64));
    }

    #[test]
    fn assistant_thinking_replays_only_while_thinking_is_enabled() {
        let make = |thinking| ModelRequest {
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "toolu_01A".into(),
                    name: "echo".into(),
                    args: json!({}),
                    provider_data: json!({ "thinking": "hmm let me see", "signature": "sig" }),
                })],
            }],
            tools: Vec::new(),
            params: ChatParams {
                model: "claude-sonnet-4-5".into(),
                temperature: None,
                max_tokens: None,
                thinking,
            },
        };

        // Enabled: the signed trace rides ahead of the call that produced
        // it, so the model continues where it left off.
        let body = serde_json::to_value(request_body(&make(ThinkingEffort::Medium)).unwrap())
            .expect("body serializes");
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                { "type": "thinking", "thinking": "hmm let me see", "signature": "sig" },
                { "type": "tool_use", "id": "toolu_01A", "name": "echo", "input": {} }
            ])
        );

        // Disabled: the API rejects thinking blocks on plain turns, so the
        // trace is dropped.
        let body = serde_json::to_value(request_body(&make(ThinkingEffort::Default)).unwrap())
            .expect("body serializes");
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                { "type": "tool_use", "id": "toolu_01A", "name": "echo", "input": {} }
            ])
        );
    }

    #[test]
    fn request_body_maps_roles_tools_and_params() {
        let req = ModelRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message::text(Role::User, "list files"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Text("checking".into()),
                        Content::ToolCall(ToolCall {
                            id: "call_0".into(),
                            name: "list_dir".into(),
                            args: json!({ "path": "." }),
                            provider_data: Value::Null,
                        }),
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(ToolResult::success(
                        "call_0",
                        "[\"src\"]",
                    ))],
                },
                Message::text(Role::User, "thanks"),
            ],
            tools: vec![ToolSpec::new(
                "list_dir",
                "lists a directory",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            )],
            params: ChatParams {
                thinking: ThinkingEffort::Default,
                model: "claude-sonnet-4-5".into(),
                temperature: Some(0.2),
                max_tokens: Some(256),
            },
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");

        assert_eq!(json["model"], "claude-sonnet-4-5");
        assert_eq!(json["max_tokens"], json!(256));
        assert_eq!(json["temperature"], json!(0.2_f32));
        // The system prompt is a block array whose end is a cache
        // breakpoint: it is byte-stable for the session's life.
        assert_eq!(
            json["system"],
            json!([{ "type": "text", "text": "be terse",
                     "cache_control": { "type": "ephemeral" } }])
        );
        assert_eq!(json["stream"], json!(true));
        assert_eq!(
            json["tools"][0],
            json!({
                "name": "list_dir",
                "description": "lists a directory",
                "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
                "cache_control": { "type": "ephemeral" }
            })
        );

        // The tool result and the following user turn merge into one user
        // message, results first.
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        // The second-to-last user turn ends a cached prefix too: the
        // rolling pair is what the next request's lookup reads.
        assert_eq!(
            messages[0]["content"],
            json!([{ "type": "text", "text": "list files",
                     "cache_control": { "type": "ephemeral" } }])
        );
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(
            messages[1]["content"],
            json!([
                { "type": "text", "text": "checking" },
                { "type": "tool_use", "id": "call_0", "name": "list_dir", "input": { "path": "." } }
            ])
        );
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"],
            json!([
                { "type": "tool_result", "tool_use_id": "call_0", "content": "[\"src\"]", "is_error": false },
                { "type": "text", "text": "thanks",
                  "cache_control": { "type": "ephemeral" } }
            ]),
            "the final block ends the cached prefix"
        );
    }

    #[test]
    fn the_last_two_user_turns_each_end_a_cached_prefix() {
        // Two tool rounds: the wire carries user, assistant, user,
        // assistant, user — the marked pair is the second and the last
        // user turn, and the turns outside the pair carry no breakpoint.
        let round = |id: &str| {
            vec![
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: id.into(),
                        name: "list_dir".into(),
                        args: json!({ "path": "." }),
                        provider_data: Value::Null,
                    })],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(ToolResult::success(id, "rows"))],
                },
            ]
        };
        let mut history = vec![Message::text(Role::User, "list files")];
        history.extend(round("c0"));
        history.extend(round("c1"));
        let req = ModelRequest {
            system: Some("be terse".into()),
            messages: history,
            tools: vec![ToolSpec::new(
                "list_dir",
                "lists a directory",
                json!({ "type": "object" }),
            )],
            params: ChatParams::new("claude-sonnet-4-5"),
        };
        let json = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 5);
        let marked: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block.get("cache_control").is_some())
                })
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(marked, vec![2, 4], "the last two user turns alone");
    }

    #[test]
    fn a_single_user_turn_carries_one_message_breakpoint() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };
        let json = serde_json::to_value(request_body(&req).unwrap()).unwrap();
        assert_eq!(
            json["messages"][0]["content"],
            json!([{ "type": "text", "text": "hi",
                     "cache_control": { "type": "ephemeral" } }])
        );
    }

    #[test]
    fn tool_results_and_user_text_group_into_one_user_message() {
        let req = ModelRequest {
            system: None,
            messages: vec![
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(ToolResult::success("call_0", "a"))],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(ToolResult::failure("call_1", "b"))],
                },
                Message::text(Role::User, "thanks"),
            ],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        let messages = json["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"],
            json!([
                { "type": "tool_result", "tool_use_id": "call_0", "content": "a", "is_error": false },
                { "type": "tool_result", "tool_use_id": "call_1", "content": "b", "is_error": true },
                { "type": "text", "text": "thanks",
                  "cache_control": { "type": "ephemeral" } }
            ])
        );
    }

    #[test]
    fn a_user_turn_with_text_and_results_hoists_the_results() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    Content::Text("and then".into()),
                    Content::ToolResult(ToolResult::failure("call_9", "boom")),
                ],
            }],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["messages"][0]["content"],
            json!([
                { "type": "tool_result", "tool_use_id": "call_9", "content": "boom", "is_error": true },
                { "type": "text", "text": "and then",
                  "cache_control": { "type": "ephemeral" } }
            ])
        );
    }

    #[test]
    fn system_messages_fold_into_the_system_prompt() {
        let req = ModelRequest {
            system: Some("a".into()),
            messages: vec![
                Message::text(Role::System, "b"),
                Message::text(Role::User, "hi"),
            ],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        // Folded system turns land in one marked block array.
        assert_eq!(
            json["system"],
            json!([{ "type": "text", "text": "a\n\nb",
                     "cache_control": { "type": "ephemeral" } }])
        );
        assert_eq!(json["messages"].as_array().expect("messages").len(), 1);
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn the_default_max_tokens_applies_when_unset() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(json["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert!(json.get("temperature").is_none());
        assert!(json.get("system").is_none());
        assert_eq!(json["stream"], json!(true));
    }

    #[test]
    fn non_object_call_args_are_wrapped_and_missing_args_cleared() {
        let req = ModelRequest {
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: "call_0".into(),
                        name: "odd".into(),
                        args: json!("not an object"),
                        provider_data: Value::Null,
                    })],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "bare".into(),
                        args: Value::Null,
                        provider_data: Value::Null,
                    })],
                },
            ],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        // Consecutive assistant turns merge into one wire message, calls
        // in order.
        let messages = json["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"][0]["input"],
            json!({ "_rhema_args": "not an object" })
        );
        assert_eq!(messages[0]["content"][1]["input"], json!({}));
    }

    #[test]
    fn a_non_object_tool_schema_degrades_to_the_empty_schema() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![ToolSpec::new("loose", "no schema", json!("not a schema"))],
            params: ChatParams::new("claude-sonnet-4-5"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["tools"][0]["input_schema"],
            json!({ "type": "object" })
        );
    }

    #[test]
    fn invalid_requests_fail_before_the_wire() {
        let no_model = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: ChatParams::new(""),
        };
        assert!(matches!(
            request_body(&no_model),
            Err(ProviderError::InvalidRequest(_))
        ));

        let no_messages = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::System, "only a system prompt")],
            tools: vec![],
            params: ChatParams::new("claude-sonnet-4-5"),
        };
        assert!(matches!(
            request_body(&no_messages),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn a_text_stream_assembles_end_turn_with_split_usage() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":3}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"type":"ping"}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        assert_eq!(
            out.message,
            Message {
                role: Role::Assistant,
                // One text block: its deltas accumulate into one content
                // block of the assembled message.
                content: vec![Content::Text("Hello".into())],
            }
        );
        assert_eq!(out.stop_reason, StopReason::EndTurn);
        // input from message_start (plus its cache reads: the whole
        // prompt), output from message_delta, cache from message_start:
        // the frames carry disjoint accounting.
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: 13,
                output_tokens: 2,
                cached_input_tokens: 3,
            }
        );
        // The tally streams live: one Usage with the prompt from
        // message_start, one with the completion from message_delta.
        // Pings and block/message stops emit nothing.
        assert_eq!(
            sink.0,
            vec![
                "Usage(Usage { input_tokens: 13, output_tokens: 1, cached_input_tokens: 3 })",
                "TextDelta(\"Hel\")",
                "TextDelta(\"lo\")",
                "Usage(Usage { input_tokens: 13, output_tokens: 2, cached_input_tokens: 3 })",
            ]
        );
    }

    #[test]
    fn a_cached_prompt_reports_its_whole_size() {
        // The prefix served from cache rides cache_read_input_tokens,
        // outside the API's own input_tokens. The normalized total must
        // still describe the whole prompt — a gauge reading the raw
        // input_tokens alone collapses to the new tokens once the
        // prefix is cached.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"message_start","message":{"id":"msg_3","type":"message","role":"assistant","usage":{"input_tokens":149,"output_tokens":1,"cache_creation_input_tokens":2000,"cache_read_input_tokens":20000}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        assert_eq!(
            out.usage,
            Usage {
                input_tokens: 22_149,
                output_tokens: 2,
                cached_input_tokens: 20_000,
            }
        );
    }

    #[test]
    fn tool_use_blocks_assemble_from_json_delta_chunks() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":5}}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01A","name":"echo","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "toolu_01A".into(),
                name: "echo".into(),
                args: json!({ "x": 1 }),
                provider_data: Value::Null,
            })]
        );
        assert_eq!(
            sink.0,
            vec![
                "Usage(Usage { input_tokens: 5, output_tokens: 0, cached_input_tokens: 0 })"
                    .to_owned(),
                "ToolCallStart { id: \"toolu_01A\", name: \"echo\" }".to_owned(),
                "ToolCallArgsDelta { id: \"toolu_01A\", delta: \"{\\\"x\\\":\" }".to_owned(),
                "ToolCallArgsDelta { id: \"toolu_01A\", delta: \"1}\" }".to_owned(),
                "Usage(Usage { input_tokens: 5, output_tokens: 3, cached_input_tokens: 0 })"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn an_idless_tool_block_gets_a_synthesized_id_and_bad_args_are_wrapped() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"odd","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"not json"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_2".into(),
                name: "odd".into(),
                args: json!({ "_anthropic_args_text": "not json" }),
                provider_data: Value::Null,
            })]
        );
    }

    #[test]
    fn blocks_keep_their_stream_order_and_empty_text_is_dropped() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_b","name":"b","input":{}}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"after"}}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_d","name":"d","input":{}}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        let kinds: Vec<String> = out
            .message
            .content
            .iter()
            .map(|content| match content {
                Content::Text(text) => format!("text:{text}"),
                Content::ToolCall(call) => format!("call:{}", call.id),
                Content::ToolResult(_) => "result".to_owned(),
            })
            .collect();
        assert_eq!(kinds, ["call:toolu_b", "text:after", "call:toolu_d"]);
    }

    #[test]
    fn unknown_block_kinds_and_deltas_are_dropped() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"oracle","x":1}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"oracle_delta","x":"hmm"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"answer"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"future_event"}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        assert_eq!(out.message.content, vec![Content::Text("answer".into())]);
        assert_eq!(out.stop_reason, StopReason::EndTurn);
        assert_eq!(sink.0, vec!["TextDelta(\"answer\")".to_owned()]);
    }

    #[test]
    fn thinking_deltas_stream_but_never_reach_the_message() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me see"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        // The trace streams live but the message carries only the reply.
        assert_eq!(out.message.content, vec![Content::Text("answer".into())]);
        assert_eq!(
            sink.0,
            vec![
                "ThinkingDelta(\"hmm \")".to_owned(),
                "ThinkingDelta(\"let me see\")".to_owned(),
                "TextDelta(\"answer\")".to_owned(),
            ]
        );
    }

    #[test]
    fn a_signed_thinking_block_rides_the_next_tool_call_for_replay() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me see"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01A","name":"echo","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        // The trace never joins the message; it rides the call it led to,
        // signature and all, ready for replay.
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "toolu_01A".into(),
                name: "echo".into(),
                args: json!({}),
                provider_data: json!({ "thinking": "hmm let me see", "signature": "sig" }),
            })]
        );
        assert_eq!(out.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn text_keeps_the_trace_pending_and_a_call_spends_it() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"note"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"a","input":{}}}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"b","input":{}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        // Text never consumes the trace; the first call claims it and a
        // later call cannot inherit it.
        assert_eq!(
            out.message.content,
            vec![
                Content::Text("note".into()),
                Content::ToolCall(ToolCall {
                    id: "toolu_1".into(),
                    name: "a".into(),
                    args: json!({}),
                    provider_data: json!({ "thinking": "why", "signature": "sig" }),
                }),
                Content::ToolCall(ToolCall {
                    id: "toolu_2".into(),
                    name: "b".into(),
                    args: json!({}),
                    provider_data: Value::Null,
                }),
            ]
        );
    }

    #[test]
    fn an_unsigned_thinking_block_is_never_replayed() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"a","input":{}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ] {
            assembler
                .push(event(payload), &mut sink)
                .expect("event feeds");
        }
        let out = assembler.finish();

        // The API rejects a thinking block without its signature, so an
        // unsigned trace rides as nothing rather than breaking the replay.
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "toolu_1".into(),
                name: "a".into(),
                args: json!({}),
                provider_data: Value::Null,
            })]
        );
    }

    #[test]
    fn an_error_event_fails_the_push() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(
                event(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
                &mut sink,
            )
            .expect_err("an error event is an error");
        assert_eq!(
            err,
            ProviderError::Api {
                message: "Overloaded".into(),
                status: None,
            }
        );
        assert!(sink.0.is_empty());

        // An unshaped error body falls back to the fixed raw text.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(event(r#"{"type":"error"}"#), &mut sink)
            .expect_err("unshaped error is still an error");
        assert_eq!(
            err,
            ProviderError::Api {
                message: "mid-stream error".into(),
                status: None,
            }
        );
    }

    #[test]
    fn stop_reasons_map_onto_stop_reasons() {
        let cases: &[(&str, StopReason)] = &[
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("refusal", StopReason::Other("refusal".into())),
            ("pause_turn", StopReason::Other("pause_turn".into())),
        ];
        for (reason, expected) in cases {
            let mut assembler = ChunkAssembler::new();
            let mut sink = Collector::new();
            assembler
                .push(
                    event(&format!(
                        r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}"}}}}"#
                    )),
                    &mut sink,
                )
                .expect("event feeds");
            assert_eq!(assembler.finish().stop_reason, *expected, "for {reason}");
        }

        // No stop reason at all defaults to EndTurn.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(event(r#"{"type":"message_stop"}"#), &mut sink)
            .expect("event feeds");
        assert_eq!(assembler.finish().stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn an_empty_stream_yields_an_empty_message() {
        let out = ChunkAssembler::new().finish();

        assert_eq!(out.message.content, Vec::<Content>::new());
        assert_eq!(out.stop_reason, StopReason::EndTurn);
        assert_eq!(out.usage, Usage::default());
    }

    #[test]
    fn model_pages_map_onto_gemini_rows() {
        let page: WireModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {
                        "type": "model",
                        "id": "claude-sonnet-4-5",
                        "display_name": "Claude Sonnet 4.5",
                        "created_at": "2025-09-29T00:00:00Z"
                    },
                    { "type": "model", "id": "claude-opus-4-1", "display_name": "Claude Opus 4.1" }
                ],
                "has_more": true,
                "first_id": "claude-sonnet-4-5",
                "last_id": "claude-opus-4-1"
            }"#,
        )
        .expect("models page parses");

        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-opus-4-1"));
        let models: Vec<ModelCard> = page.data.into_iter().map(Into::into).collect();

        assert_eq!(models[0].id, "claude-sonnet-4-5");
        assert_eq!(models[0].display_name, "Claude Sonnet 4.5");
        // The list carries nothing else: every catalog field defaults.
        assert_eq!(models[0].description, "");
        assert_eq!(models[0].input_token_limit, None);
        assert_eq!(models[0].output_token_limit, None);
        assert_eq!(models[0].generation_methods, vec!["generateContent"]);
        assert_eq!(models[0].pricing, None);
        assert!(models[0].supports_chat());
    }

    #[test]
    fn error_bodies_expose_the_documented_message() {
        let body: WireErrorBody = serde_json::from_str(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: field required"}}"#,
        )
        .expect("error body parses");
        assert_eq!(body.message("raw"), "max_tokens: field required");

        let unshaped: WireErrorBody = serde_json::from_str("not even json").unwrap_or_default();
        assert_eq!(unshaped.message("not even json"), "not even json");
    }

    #[test]
    fn the_zai_quota_reads_the_windows_left_share() {
        let quota: WireZaiQuota = serde_json::from_str(
            r#"{
                "code": 200,
                "msg": "Operation successful",
                "success": true,
                "data": {
                    "level": "PRO",
                    "limits": [
                        { "type": "TIME_LIMIT", "unit": 5, "percentage": 38,
                          "currentValue": 38, "usage": 100, "remaining": 62 },
                        { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 47 },
                        { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 12 }
                    ]
                }
            }"#,
        )
        .expect("quota response parses");

        // Both coding windows ride as the share still unspent, the
        // 5-hour one first; the tool quota stays out.
        assert_eq!(
            allowance(&quota),
            Some(Allowance {
                amount: "5h 53% wk 88%".to_owned(),
                kind: String::new(),
            })
        );
    }

    #[test]
    fn credit_metered_plans_read_the_same_windows() {
        let quota: WireZaiQuota = serde_json::from_str(
            r#"{
                "success": true,
                "data": { "limits": [
                    { "type": "CREDIT_LIMIT", "unit": 3, "number": 5, "percentage": 99.6 },
                    { "type": "CREDIT_LIMIT", "unit": 6, "number": 1, "percentage": 100 }
                ] }
            }"#,
        )
        .expect("credit quota response parses");

        // The left share clamps at zero: an overdrawn window never
        // renders a negative badge.
        assert_eq!(
            allowance(&quota),
            Some(Allowance {
                amount: "5h 0% wk 0%".to_owned(),
                kind: String::new(),
            })
        );
    }

    #[test]
    fn a_quota_report_without_coding_windows_yields_no_allowance() {
        let none: WireZaiQuota =
            serde_json::from_str(r#"{ "success": true, "data": { "limits": [] } }"#)
                .expect("empty quota response parses");
        assert_eq!(allowance(&none), None);

        let error: WireZaiQuota = serde_json::from_str(
            r#"{ "code": 401, "msg": "token expired or incorrect", "success": false }"#,
        )
        .expect("error envelope parses");
        assert_eq!(allowance(&error), None);
    }
}
