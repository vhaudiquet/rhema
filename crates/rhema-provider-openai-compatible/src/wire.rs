//! OpenAI-compatible wire types and the pure mapping between them and
//! rhema-core's model. No I/O here — [`crate::provider`] drives these
//! functions.
//!
//! This is the generic dialect: the same shape OpenAI's API speaks and the
//! compatible servers imitate (vLLM, LM Studio, llama.cpp's OpenAI mode,
//! proxies). Where a server's behavior is unknown, the mapping tolerates
//! the widest set of dialects rather than specializing on one. Mapping
//! constraints (the wire has a role for everything rhema-core has):
//! - `model` is passed through verbatim and never validated here: the
//!   server owns rejection, so a malformed request surfaces as a 400 mapped
//!   to [`ProviderError::Api`](rhema_core::ProviderError::Api).
//! - Roles map one-to-one. `ModelRequest.system` becomes a leading `system`
//!   message; each `Role::System` message becomes its own `system` message
//!   (text blocks joined by a blank line). `Role::User` and `Role::Assistant`
//!   map to their like-named roles; text blocks of one message are joined by
//!   a blank line into the single `content` string.
//! - An assistant turn with text and/or tool calls becomes ONE wire message:
//!   `content` is omitted when there is no text, `tool_calls` when there are
//!   no calls. Servers reject assistant messages with neither, so an empty
//!   assistant turn is dropped from history.
//! - Tool results become `{role: "tool", tool_call_id, content}` messages,
//!   one per result block, matched by id — whichever rhema message carries
//!   them. `ToolResult::is_error` has no wire representation; the content
//!   text alone must tell the model the call failed.
//! - `ToolCall::provider_data` carries only the reasoning the call claims
//!   from the stream; OpenAI-style call ids are plain strings that
//!   round-trip on the wire, so there is nothing else opaque to carry.
//! - Tool specs map to `[{type: "function", function: {name, description,
//!   parameters}}]`, with `parameters` passed through verbatim.
//! - Every call streams (`stream: true`) with
//!   `stream_options: {include_usage: true}` — without the flag OpenAI and
//!   most compatible servers send no usage at all in a streamed response.
//!   Servers that ignore `stream_options` simply omit the usage chunk; the
//!   mapping then degrades to zero usage, never an error.
//! - The output token ceiling rides in `max_tokens` — the field the sibling
//!   adapters send and the one every compatible server accepts. OpenAI's
//!   newer `max_completion_tokens` is deliberately not used: generic
//!   servers predate it.
//! - A named thinking effort rides in `reasoning_effort`; absent, the
//!   server's default stands.
//! - Streaming chunks: `delta.content` becomes one `TextDelta` per
//!   non-empty piece. `delta.tool_calls[]` fragments are keyed by `index`;
//!   the FIRST fragment of an index emits `ToolCallStart` (the wire id, or a
//!   synthesized `call_{index}` when absent) and every non-empty
//!   `function.arguments` fragment emits `ToolCallArgsDelta`. Id and name
//!   arrive on the first fragment of an index and argument text in pieces;
//!   the assembler never assumes both arrived together.
//! - The assembled message carries the streamed text as one part — chunk
//!   boundaries are transport artifacts, and per-chunk parts would be
//!   replayed next request joined by blank lines, teaching the model to
//!   emit `token\n\ntoken` prose — then tool calls
//!   in ascending index order. A call's arguments are the concatenated
//!   fragments parsed as JSON; an unparseable payload wraps as
//!   `{"_openai_args_text": <raw>}` — a malformed call must not fail the
//!   turn. Empty arguments become `{}`.
//! - `finish_reason` maps onto rhema-core's stop reason, with any tool call
//!   in the assembled message winning as
//!   [`StopReason::ToolUse`](rhema_core::StopReason::ToolUse) — the same
//!   rule as the sibling adapters. The legacy `"function"` reason maps to
//!   `ToolUse` beside `"tool_calls"`.
//! - `delta.reasoning_content` becomes one `ThinkingDelta` per non-empty
//!   piece; reasoning streamed before a tool call is claimed by that call
//!   and replayed on assistant messages via `reasoning_content`. Other
//!   provider-specific delta fields have no rhema-core counterpart and are
//!   ignored.
//! - Errors: a non-2xx body is `{"error": {code, message, type}}`; the
//!   mapper keeps `error.message` and falls back to the raw body. A server
//!   can also fail mid-stream with a `data: {"error": ...}` chunk (then
//!   closing the stream), which surfaces as the same
//!   [`ProviderError::Api`](rhema_core::ProviderError::Api).
//! - Models: `GET {base}/models` returns `{"data": [{...}]}`. The generic
//!   listing guarantees only `data[].id`; `name`, `description`, and
//!   `context_length` are read when present and everything else is
//!   defaulted — no field is invented.

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

/// Marker key wrapping an unparseable streamed arguments payload so the
/// call still round-trips through history.
const WRAPPED_ARGS_KEY: &str = "_openai_args_text";

// --- Request wire types (serialize) ---

/// The `POST {base}/chat/completions` request body. Always streaming, with
/// the usage-only final chunk requested.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatCompletionRequest {
    /// Model identifier, passed through verbatim.
    pub model: String,
    /// Conversation turns.
    pub messages: Vec<WireMessage>,
    /// Tools offered this step, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    /// Always `true`: the provider only speaks the streaming dialect.
    pub stream: bool,
    /// Always requests the usage-only final chunk; without it OpenAI-shaped
    /// servers send no token accounting on a streamed call.
    pub stream_options: WireStreamOptions,
    /// Sampling temperature, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// How much reasoning the model should do, when the driver named a
    /// level; absent means the server's default stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
}

/// The `stream_options` request field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WireStreamOptions {
    /// Ask for the usage-only final chunk.
    pub include_usage: bool,
}

/// One message on the wire. A single wire message carries either a text
/// `content`, or an assistant's `tool_calls`, or a tool result's
/// `tool_call_id` — exactly one of the three is set by the mapper.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessage {
    /// `"system"`, `"user"`, `"assistant"`, or `"tool"`.
    pub role: String,
    /// The turn's text, when it has any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The assistant's requested tool calls, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    /// The reasoning that led to this assistant turn, replayed verbatim
    /// from history: DeepSeek-style servers continue the chain of thought
    /// across tool calls instead of re-deriving it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// The answered call's id, on `role: "tool"` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A tool call inside assistant history: id and name round-trip verbatim,
/// arguments as their JSON text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireToolCall {
    /// The call's correlation id, echoed from history.
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The called function.
    pub function: WireFunctionCall,
}

/// The `function` object of a history [`WireToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunctionCall {
    /// Name of the called tool.
    pub name: String,
    /// Call arguments as their JSON text.
    pub arguments: String,
}

/// A `tools[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The advertised function.
    pub function: WireFunctionSpec,
}

/// The function a [`WireTool`] advertises.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunctionSpec {
    /// Name the model uses to call the tool.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The tool's argument schema, passed through verbatim.
    pub parameters: Value,
}

// --- Response wire types (deserialize) ---

/// One streamed `chat.completion.chunk`, or the error object a server
/// sends in place of one when the call fails mid-stream.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ChatCompletionChunk {
    /// Chunk deltas; empty on the usage-only final chunk.
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    /// Token accounting; only on the final chunk, and only when the server
    /// honors `stream_options.include_usage`.
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// The server's error detail, on a stream aborted by a failure: the
    /// chunk body is the whole `{"error": {...}}` wrapper, so this field
    /// holds the detail directly.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// One choice of a chunk; compatible servers send exactly one per content
/// chunk, the usage chunk sends none.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireChoice {
    /// The piece generated since the last chunk, when any.
    #[serde(default)]
    pub delta: Option<WireDelta>,
    /// Why generation stopped; `null` on every chunk but the last content
    /// one.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The `delta` object of a [`WireChoice`]. Fields beyond `content`,
/// `tool_calls`, and `reasoning_content` (role echoes, unknown extras) are
/// ignored.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireDelta {
    /// A piece of assistant text.
    #[serde(default)]
    pub content: Option<String>,
    /// A piece of the model's reasoning trace (the DeepSeek/GLM
    /// convention, served by most thinking models on OpenAI-shaped
    /// endpoints). Emitted as [`Event::ThinkingDelta`], never assembled
    /// into the message.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Tool-call fragments, keyed by their `index`.
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallDelta>>,
}

/// One tool-call fragment of a [`WireDelta`]. The first fragment of an
/// index carries `id` and the function `name`; continuation fragments carry
/// only argument text — never assume both arrive together.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireToolCallDelta {
    /// Which call of the message this fragment belongs to.
    #[serde(default)]
    pub index: u32,
    /// The call's correlation id, on the first fragment.
    #[serde(default)]
    pub id: Option<String>,
    /// Name and argument pieces, when the fragment carries any.
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

/// The `function` object of a [`WireToolCallDelta`].
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireFunctionDelta {
    /// The tool's name, on the first fragment.
    #[serde(default)]
    pub name: Option<String>,
    /// A piece of the arguments JSON text.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// The `usage` object of the final chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireUsage {
    /// Prompt tokens billed.
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    /// Completion tokens billed.
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    /// Prompt cache accounting, when the server reports it.
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
    /// DeepSeek names its KV-cache split at the top level: the prompt
    /// subset served from the cache, and the subset that missed it. Both
    /// are included in `prompt_tokens`.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// The prompt subset that missed the cache.
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
}

/// The `prompt_tokens_details` object of a [`WireUsage`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WirePromptTokensDetails {
    /// Prompt subset served from the server's prompt cache.
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// The `error` object of a non-2xx response body, or of a mid-stream error
/// chunk: `{"error": {"code": ..., "message": ..., "type": ...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireErrorBody {
    /// The error detail, when the body has the documented shape.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// OpenAI-style error detail. The `code` is a number on HTTP errors but a
/// string on some servers' mid-stream ones, so it is kept uninterpreted;
/// only `message` is ever surfaced.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireErrorDetail {
    /// Number or string error code, per the shape above.
    #[serde(default)]
    pub code: Option<Value>,
    /// Human-readable description.
    #[serde(default)]
    pub message: Option<String>,
    /// OpenAI-style error type (e.g. `invalid_request_error`).
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
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

/// Normalizes a `GET {base}/models` page into the shared model rows. The
/// generic listing guarantees only `data[].id`: `name`, `description`, and
/// `context_length` are read when present, everything else is defaulted,
/// and entries without a usable id are dropped.
pub fn normalize_models(page: &serde_json::Value) -> Vec<ModelCard> {
    let entries = page
        .get("data")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Some(ModelCard {
                id,
                display_name: text("name"),
                description: text("description"),
                input_token_limit: entry.get("context_length").and_then(Value::as_u64),
                // The generic listing reports one shared context window and
                // no per-direction output ceiling.
                output_token_limit: None,
                // Every listed model is offered through chat completions.
                generation_methods: vec!["generateContent".to_owned()],
                // The generic listing carries no prices; adapters with a
                // richer catalog map their own.
                pricing: None,
            })
        })
        .collect()
}

// --- Allowance: the DeepSeek balance endpoint -> normalized ---

/// The `GET /user/balance` response of DeepSeek — the one account
/// endpoint this adapter knows.
#[derive(Debug, Default, Deserialize)]
pub struct WireBalance {
    /// One entry per currency the account holds.
    #[serde(default)]
    pub balance_infos: Vec<WireBalanceInfo>,
}

/// One currency's balance of a [`WireBalance`].
#[derive(Debug, Default, Deserialize)]
pub struct WireBalanceInfo {
    /// The ISO code: `"USD"`, `"CNY"`.
    #[serde(default)]
    pub currency: String,
    /// The total balance, as a decimal string.
    #[serde(default)]
    pub total_balance: String,
}

/// The account's prepaid balance, the first currency entry winning —
/// the account charges in one. `None` when the body lists no parseable
/// balance.
pub fn allowance(balance: &WireBalance) -> Option<Allowance> {
    let info = balance.balance_infos.first()?;
    let total: f64 = info.total_balance.parse().ok()?;
    let amount = match info.currency.as_str() {
        "USD" => format!("${total:.2}"),
        "CNY" => format!("¥{total:.2}"),
        other => format!("{total:.2} {other}"),
    };
    Some(Allowance {
        amount,
        kind: String::new(),
    })
}

/// Whether the model is a text chat model. A generic server serves
/// whatever is loaded into it — embedding, reranking, and speech models
/// behind the same `/models` page as the chat models — and only the ids
/// tell them apart; those are excluded.
pub fn is_text(model: &ModelCard) -> bool {
    const NON_TEXT: [&str; 8] = [
        "embed",
        "rerank",
        "whisper",
        "tts",
        "asr",
        "audio",
        "transcribe",
        "moderation",
    ];
    !NON_TEXT.iter().any(|marker| model.id.contains(marker))
}

// --- Mapping: ModelRequest -> wire ---

/// Builds the chat-completions request body for `req`. The model is passed
/// through verbatim and nothing is validated: the server owns rejection.
pub fn request_body(req: &ModelRequest) -> ChatCompletionRequest {
    let mut messages: Vec<WireMessage> = Vec::new();
    if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
        messages.push(WireMessage {
            role: "system".into(),
            content: Some(system.to_owned()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for message in &req.messages {
        match message.role {
            Role::System => {
                let text = joined_texts(message);
                if !text.is_empty() {
                    messages.push(WireMessage {
                        role: "system".into(),
                        content: Some(text),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            Role::User => {
                let text = joined_texts(message);
                if !text.is_empty() {
                    messages.push(WireMessage {
                        role: "user".into(),
                        content: Some(text),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                messages.extend(tool_results_of(message));
            }
            Role::Assistant => {
                let text = joined_texts(message);
                let calls: Vec<WireToolCall> = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call) => Some(WireToolCall {
                            id: call.id.clone(),
                            kind: "function".into(),
                            function: WireFunctionCall {
                                name: call.name.clone(),
                                arguments: arguments_text(&call.args),
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                // The reasoning that led to the calls, replayed in order:
                // the blocks must match what the model generated.
                let reasoning = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call) => call
                            .provider_data
                            .get("reasoning")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        _ => None,
                    })
                    .collect::<Vec<String>>()
                    .join("\n\n");
                // Servers reject an assistant message with neither content
                // nor tool_calls.
                if !text.is_empty() || !calls.is_empty() {
                    messages.push(WireMessage {
                        role: "assistant".into(),
                        content: (!text.is_empty()).then_some(text),
                        tool_calls: (!calls.is_empty()).then_some(calls),
                        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                        tool_call_id: None,
                    });
                }
                messages.extend(tool_results_of(message));
            }
            Role::Tool => {
                // The tool role carries only results; stray text blocks have
                // no wire home and are dropped.
                messages.extend(tool_results_of(message));
            }
        }
    }

    let tools = (!req.tools.is_empty()).then(|| {
        req.tools
            .iter()
            .map(|spec| WireTool {
                kind: "function".into(),
                function: WireFunctionSpec {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    parameters: spec.parameters.clone(),
                },
            })
            .collect()
    });

    ChatCompletionRequest {
        model: req.params.model.clone(),
        messages,
        tools,
        stream: true,
        stream_options: WireStreamOptions {
            include_usage: true,
        },
        temperature: req.params.temperature,
        max_tokens: req.params.max_tokens,
        reasoning_effort: reasoning_effort(req.params.thinking),
    }
}

/// The OpenAI `reasoning_effort` spelling of a thinking level; `None`
/// sends nothing and the server's default stands.
fn reasoning_effort(thinking: ThinkingEffort) -> Option<&'static str> {
    match thinking {
        ThinkingEffort::Default => None,
        ThinkingEffort::Low => Some("low"),
        ThinkingEffort::Medium => Some("medium"),
        ThinkingEffort::High => Some("high"),
    }
}

/// The text blocks of a message, joined by a blank line.
fn joined_texts(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One `role: "tool"` wire message per result block of `message`.
fn tool_results_of(message: &Message) -> impl Iterator<Item = WireMessage> + '_ {
    message.content.iter().filter_map(|content| match content {
        Content::ToolResult(result) => Some(WireMessage {
            role: "tool".into(),
            content: Some(result.content.clone()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(result.id.clone()),
        }),
        _ => None,
    })
}

/// Call arguments as their JSON text; a `null` payload becomes `{}`.
fn arguments_text(args: &Value) -> String {
    if args.is_null() {
        "{}".to_owned()
    } else {
        args.to_string()
    }
}

// --- Mapping: wire chunks -> ModelOutput / events ---

/// One streamed tool call being assembled across fragments.
#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    started: bool,
    /// The reasoning streamed since the previous call or segment: the
    /// chain of thought that led to this tool request, replayed so the
    /// model continues where it left off.
    reasoning: String,
}

/// Assembles streamed chunks into the final [`ModelOutput`], emitting
/// streaming events as deltas arrive.
///
/// `finish_reason` and `usage` may arrive on separate chunks (the dialect
/// sends a `delta: {}` chunk with the reason, then a choices-less usage
/// chunk); the last value seen wins. When the assembled message contains
/// tool calls, the effective stop reason is
/// [`StopReason::ToolUse`] regardless of the wire value.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    /// The assistant text streamed so far. One buffer, not one entry per
    /// chunk: a chunk boundary is a transport artifact, and a per-chunk
    /// part list would be replayed next turn as `token\n\ntoken` — the
    /// model then mimics that shredded format in its own replies.
    text: String,
    /// The reasoning streamed since the last tool call began; claimed by
    /// the next call's first fragment.
    reasoning: String,
    calls: BTreeMap<u32, PartialCall>,
    finish_reason: Option<String>,
    usage: Usage,
}

impl ChunkAssembler {
    /// An assembler that has seen no chunks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk, emitting its streaming events. Fails with
    /// [`ProviderError::Api`] when the chunk is the server's mid-stream
    /// error object.
    pub fn push(
        &mut self,
        chunk: ChatCompletionChunk,
        sink: &mut dyn EventSink,
    ) -> Result<(), ProviderError> {
        if let Some(error) = &chunk.error {
            return Err(ProviderError::Api(
                error
                    .message
                    .clone()
                    .unwrap_or_else(|| "mid-stream error".to_owned()),
            ));
        }
        if let Some(usage) = chunk.usage {
            self.usage = Usage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
                cached_input_tokens: usage
                    .prompt_tokens_details
                    .and_then(|details| details.cached_tokens)
                    .or(usage.prompt_cache_hit_tokens)
                    .unwrap_or(0),
            };
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
            let Some(delta) = choice.delta else {
                continue;
            };
            if let Some(text) = delta.reasoning_content.filter(|text| !text.is_empty()) {
                sink.emit(Event::ThinkingDelta(&text));
                self.reasoning.push_str(&text);
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                sink.emit(Event::TextDelta(&text));
                self.text.push_str(&text);
            }
            for fragment in delta.tool_calls.into_iter().flatten() {
                self.push_fragment(fragment, sink);
            }
        }
        Ok(())
    }

    /// Folds one tool-call fragment: the index's first fragment announces
    /// the call, every fragment with argument text feeds the args deltas.
    fn push_fragment(&mut self, fragment: WireToolCallDelta, sink: &mut dyn EventSink) {
        let index = fragment.index;
        // Reasoning streamed since the last fragment led to this call —
        // claimed by its first fragment, appended when interleaved.
        let pending_reasoning = std::mem::take(&mut self.reasoning);
        let partial = self.calls.entry(index).or_default();
        if !partial.started {
            partial.started = true;
            partial.reasoning = pending_reasoning;
            let id = fragment
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{index}"));
            let name = fragment
                .function
                .as_ref()
                .and_then(|function| function.name.as_deref())
                .filter(|name| !name.is_empty())
                .unwrap_or_default()
                .to_owned();
            partial.id = Some(id.clone());
            partial.name = Some(name.clone());
            sink.emit(Event::ToolCallStart {
                id: &id,
                name: &name,
            });
        } else if !pending_reasoning.is_empty() {
            partial.reasoning.push_str(&pending_reasoning);
        }
        if let Some(piece) = fragment
            .function
            .and_then(|function| function.arguments)
            .filter(|piece| !piece.is_empty())
        {
            sink.emit(Event::ToolCallArgsDelta(&piece));
            partial.arguments.push_str(&piece);
        }
    }

    /// Consumes the assembler into the final output. The message is always
    /// `Role::Assistant`: the streamed text as one part (chunk boundaries
    /// are transport artifacts), then tool calls in ascending index order;
    /// an empty stream yields an empty content list, not an error.
    pub fn finish(self) -> ModelOutput {
        let mut content: Vec<Content> = Vec::new();
        if !self.text.is_empty() {
            content.push(Content::Text(self.text));
        }
        let mut has_calls = false;
        for (index, partial) in self.calls {
            has_calls = true;
            let provider_data = if partial.reasoning.is_empty() {
                Value::Null
            } else {
                serde_json::json!({ "reasoning": partial.reasoning })
            };
            content.push(Content::ToolCall(ToolCall {
                id: partial.id.unwrap_or_else(|| format!("call_{index}")),
                name: partial.name.unwrap_or_default(),
                args: parse_args(&partial.arguments),
                provider_data,
            }));
        }
        ModelOutput {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: stop_reason(self.finish_reason.as_deref(), has_calls),
            usage: self.usage,
        }
    }
}

/// Parses a call's assembled arguments text; an empty payload is `{}` and
/// an unparseable one wraps under [`WRAPPED_ARGS_KEY`] so the turn survives.
fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({ WRAPPED_ARGS_KEY: raw }))
}

/// Maps the wire `finish_reason` onto rhema-core's stop reason; any tool
/// call in the assembled message wins as [`StopReason::ToolUse`] — the same
/// rule as the sibling adapters.
fn stop_reason(finish_reason: Option<&str>, has_calls: bool) -> StopReason {
    if has_calls {
        return StopReason::ToolUse;
    }
    match finish_reason {
        None | Some("") | Some("stop") => StopReason::EndTurn,
        // "function" is the pre-tool_calls legacy reason some servers and
        // old models still emit.
        Some("tool_calls") | Some("function") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        // "content_filter" and every provider-specific reason land here.
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

    fn chunk(json: &str) -> ChatCompletionChunk {
        serde_json::from_str(json).expect("chunk fixture parses")
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
                model: "gpt-4o-mini".into(),
                temperature: Some(0.2),
                max_tokens: Some(256),
                thinking: ThinkingEffort::Default,
            },
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");

        assert_eq!(json["model"], "gpt-4o-mini");
        assert_eq!(json["stream"], json!(true));
        assert_eq!(json["stream_options"]["include_usage"], json!(true));
        assert_eq!(json["temperature"], json!(0.2_f32));
        // `max_tokens`, not `max_completion_tokens`: the field generic
        // servers accept.
        assert_eq!(json["max_tokens"], json!(256));
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "list_dir");
        assert_eq!(json["tools"][0]["function"]["parameters"]["type"], "object");

        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be terse");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "list files");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "checking");
        assert_eq!(
            messages[2]["tool_calls"][0],
            json!({
                "id": "call_0",
                "type": "function",
                "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" }
            })
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_0");
        assert_eq!(messages[3]["content"], "[\"src\"]");
        assert_eq!(messages[4]["role"], "user");
        assert_eq!(messages[4]["content"], "thanks");
    }

    #[test]
    fn system_messages_each_keep_their_own_wire_message() {
        let req = ModelRequest {
            system: Some("a".into()),
            messages: vec![
                Message::text(Role::System, "b"),
                Message::text(Role::User, "hi"),
            ],
            tools: vec![],
            params: ChatParams::new("gpt-4o-mini"),
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");
        let messages = json["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"], "a");
        assert_eq!(messages[1]["content"], "b");
        assert!(json.get("tools").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn an_assistant_turn_with_no_text_omits_content() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_0".into(),
                    name: "odd".into(),
                    args: Value::Null,
                    provider_data: Value::Null,
                })],
            }],
            tools: vec![],
            params: ChatParams::new("gpt-4o-mini"),
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");
        let messages = json["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].get("content").is_none());
        assert_eq!(messages[0]["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn an_empty_assistant_turn_is_dropped_and_user_tool_results_keep_their_role() {
        let req = ModelRequest {
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Content::Text("and then".into()),
                        Content::ToolResult(ToolResult::failure("call_9", "boom")),
                    ],
                },
            ],
            tools: vec![],
            params: ChatParams::new("gpt-4o-mini"),
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");
        let messages = json["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_9");
        assert_eq!(messages[1]["content"], "boom");
    }

    #[test]
    fn text_chunks_assemble_end_turn_with_usage() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,",
                    "\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},",
                    "\"finish_reason\":null}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk(
                    "{\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}",
                ),
                &mut sink,
            )
            .expect("chunk feeds");
        // The usage-only final chunk: no choices at all.
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,",
                    "\"total_tokens\":12,\"prompt_tokens_details\":{\"cached_tokens\":4}}}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(
            out.message,
            Message {
                role: Role::Assistant,
                content: vec![Content::Text("Hello".into())],
            }
        );
        assert_eq!(out.stop_reason, StopReason::EndTurn);
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
            }
        );
        assert_eq!(sink.0, vec!["TextDelta(\"Hel\")", "TextDelta(\"lo\")"]);
    }

    #[test]
    fn chunk_boundaries_never_reach_the_history_wire() {
        // The regression: per-chunk text parts replayed next request as
        // `Hel\n\nlo`, and the model mimicked the shredded format.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for piece in ["to", " re", " -add"] {
            assembler
                .push(
                    chunk(&format!(
                        r#"{{"choices":[{{"delta":{{"content":"{piece}"}}}}]}}"#
                    )),
                    &mut sink,
                )
                .expect("chunk feeds");
        }
        let out = assembler.finish();

        let req = ModelRequest {
            system: None,
            messages: vec![out.message, Message::text(Role::User, "go on")],
            tools: Vec::new(),
            params: ChatParams::new("gpt-4o-mini"),
        };
        let json = serde_json::to_value(request_body(&req)).expect("body serializes");
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], "to re -add");
    }

    #[test]
    fn a_stream_without_a_usage_chunk_degrades_to_zero_usage() {
        // A server that ignores stream_options sends no usage at all —
        // that is a degenerate but valid stream, not an error.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(
                    "{\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}",
                ),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(out.usage, Usage::default());
        assert_eq!(out.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn tool_call_fragments_assemble_tool_use() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        // First fragment: id + name; later fragments: argument text pieces.
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                    "\"id\":\"call_abc\",\"type\":\"function\",",
                    "\"function\":{\"name\":\"echo\",\"arguments\":\"\"}}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                    "\"function\":{\"arguments\":\"{\\\"x\\\":\"}}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                    "\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "echo".into(),
                args: json!({ "x": 1 }),
                provider_data: Value::Null,
            })]
        );
        assert_eq!(
            sink.0,
            vec![
                "ToolCallStart { id: \"call_abc\", name: \"echo\" }".to_owned(),
                "ToolCallArgsDelta(\"{\\\"x\\\":\")".to_owned(),
                "ToolCallArgsDelta(\"1}\")".to_owned(),
            ]
        );
    }

    #[test]
    fn parallel_calls_keep_their_index_order_and_first_seen_ids() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",",
                    "\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}},",
                    "{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",",
                    "\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        let ids: Vec<String> = out
            .message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_a".to_owned(), "call_b".to_owned()]);
    }

    #[test]
    fn an_idless_first_fragment_gets_a_synthesized_id_and_bad_args_are_wrapped() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2,",
                    "\"function\":{\"name\":\"odd\",\"arguments\":\"not json\"}}]},",
                    "\"finish_reason\":\"tool_calls\"}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_2".into(),
                name: "odd".into(),
                args: json!({ "_openai_args_text": "not json" }),
                provider_data: Value::Null,
            })]
        );
        assert_eq!(out.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn empty_arguments_become_an_empty_object() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                    "\"id\":\"call_0\",\"type\":\"function\",\"function\":{\"name\":\"bare\"}}]},",
                    "\"finish_reason\":\"tool_calls\"}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "bare".into(),
                args: json!({}),
                provider_data: Value::Null,
            })]
        );
    }

    #[test]
    fn a_tool_call_beats_the_wire_finish_reason() {
        // Compatible servers disagree on the reason for tool-call turns.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                    "\"id\":\"call_0\",\"type\":\"function\",",
                    "\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}}]},",
                    "\"finish_reason\":\"stop\"}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assert_eq!(assembler.finish().stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn finish_reasons_map_onto_stop_reasons() {
        let cases: &[(&str, StopReason)] = &[
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::ToolUse),
            // The legacy pre-tool_calls reason.
            ("function", StopReason::ToolUse),
            ("content_filter", StopReason::Other("content_filter".into())),
            ("server_error", StopReason::Other("server_error".into())),
        ];
        for (reason, expected) in cases {
            let mut assembler = ChunkAssembler::new();
            let mut sink = Collector::new();
            assembler
                .push(
                    chunk(&format!(
                        "{{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}"
                    )),
                    &mut sink,
                )
                .expect("chunk feeds");
            assert_eq!(assembler.finish().stop_reason, *expected, "for {reason}");
        }

        // No finish reason at all defaults to EndTurn.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk("{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
        assert_eq!(assembler.finish().stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn deepseeks_top_level_cache_split_maps_to_cached_tokens() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":2,",
                    "\"prompt_cache_hit_tokens\":80,\"prompt_cache_miss_tokens\":20}}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        // DeepSeek names the KV-cache split at the top level; the cached
        // subset is the hit side, still inside prompt_tokens.
        assert_eq!(out.usage.input_tokens, 100);
        assert_eq!(out.usage.cached_input_tokens, 80);
    }

    #[test]
    fn a_named_thinking_effort_rides_in_reasoning_effort() {
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: "gpt-4o-mini".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::High,
            },
        };
        let body = serde_json::to_string(&request_body(&req)).unwrap();
        assert!(body.contains("\"reasoning_effort\":\"high\""), "{body}");

        // Default sends nothing: strict endpoints keep their shape.
        req.params.thinking = ThinkingEffort::Default;
        let body = serde_json::to_string(&request_body(&req)).unwrap();
        assert!(!body.contains("reasoning_effort"), "{body}");
    }

    #[test]
    fn reasoning_content_streams_but_never_reaches_the_message() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        // Both fields empty: nothing emits.
        assembler
            .push(
                chunk("{\"choices\":[{\"delta\":{\"content\":\"\",\"reasoning_content\":\"\"}}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
        // A reasoning piece emits its own delta...
        assembler
            .push(
                chunk("{\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
        // ...and the reply text follows untouched by it.
        assembler
            .push(
                chunk(
                    "{\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}",
                ),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(out.message.content, vec![Content::Text("ok".into())]);
        assert_eq!(
            sink.0,
            vec![
                "ThinkingDelta(\"hmm\")".to_owned(),
                "TextDelta(\"ok\")".to_owned()
            ]
        );
    }

    #[test]
    fn assistant_reasoning_replays_from_provider_data() {
        let reasoning = "I should list the files first";
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_0".into(),
                    name: "a".into(),
                    args: json!({}),
                    provider_data: json!({ "reasoning": reasoning }),
                })],
            }],
            tools: Vec::new(),
            params: ChatParams {
                model: "gpt-4o-mini".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::Default,
            },
        };
        let body = serde_json::to_value(request_body(&req)).expect("body serializes");
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], reasoning);

        // Without stored reasoning the field stays absent.
        let plain = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: req.params.clone(),
        };
        let body = serde_json::to_value(request_body(&plain)).expect("body serializes");
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("reasoning_content").is_none()),
            "{body}"
        );
    }

    #[test]
    fn reasoning_content_before_a_tool_call_is_kept_for_replay() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"choices":[{"delta":{"reasoning_content":"I should list "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"the files first"}}]}"#,
            concat!(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"#,
                r#""id":"call_0","type":"function","#,
                r#""function":{"name":"a","arguments":"{}"}}]}}]}"#
            ),
        ] {
            assembler
                .push(chunk(payload), &mut sink)
                .expect("chunk feeds");
        }
        let out = assembler.finish();

        // The reasoning rides in the call's provider data for replay; it
        // never joins the message text.
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "a".into(),
                args: json!({}),
                provider_data: json!({ "reasoning": "I should list the files first" }),
            })]
        );
    }

    #[test]
    fn last_finish_reason_and_usage_win() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],",
                    "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":0}}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":7,",
                    "\"prompt_tokens_details\":{\"cached_tokens\":3}}}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(out.stop_reason, StopReason::EndTurn);
        assert_eq!(
            out.usage,
            Usage {
                input_tokens: 9,
                output_tokens: 7,
                cached_input_tokens: 3,
            }
        );
    }

    #[test]
    fn a_mid_stream_error_chunk_fails_the_push() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(
                chunk(concat!(
                    "{\"error\":{\"code\":500,\"message\":\"inference failed\",",
                    "\"type\":\"server_error\"}}"
                )),
                &mut sink,
            )
            .expect_err("mid-stream error is an error");
        assert_eq!(err, ProviderError::Api("inference failed".into()));
        assert!(sink.0.is_empty());

        // An unshaped error body falls back to the fixed raw text.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(chunk("{\"error\":{}}"), &mut sink)
            .expect_err("unshaped error is still an error");
        assert_eq!(err, ProviderError::Api("mid-stream error".into()));
    }

    #[test]
    fn error_bodies_expose_the_documented_message() {
        let body: WireErrorBody = serde_json::from_str(
            "{\"error\":{\"code\":400,\"message\":\"invalid request\",\"type\":\"invalid_request_error\"}}",
        )
        .expect("error body parses");
        assert_eq!(body.message("raw"), "invalid request");

        let unshaped: WireErrorBody = serde_json::from_str("not even json").unwrap_or_default();
        assert_eq!(unshaped.message("not even json"), "not even json");
    }

    #[test]
    fn models_page_normalizes_sparse_rows() {
        let page: Value = serde_json::from_str(
            r#"{
                "object": "list",
                "data": [
                    { "id": "gpt-4o-mini", "object": "model", "owned_by": "openai" },
                    {
                        "id": "local-model",
                        "name": "Local",
                        "description": "a vLLM entry",
                        "context_length": 32768
                    },
                    { "object": "model" }
                ]
            }"#,
        )
        .expect("models page parses");
        let models = normalize_models(&page);

        // The id-less entry is dropped; the sparse one is defaulted.
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o-mini");
        assert_eq!(models[0].display_name, "");
        assert_eq!(models[0].input_token_limit, None);
        assert_eq!(models[0].output_token_limit, None);
        assert_eq!(models[0].generation_methods, vec!["generateContent"]);
        assert_eq!(models[0].pricing, None);
        assert_eq!(models[1].id, "local-model");
        assert_eq!(models[1].display_name, "Local");
        assert_eq!(models[1].input_token_limit, Some(32_768));
    }

    #[test]
    fn non_text_modalities_are_excluded_by_id() {
        let text = |id: &str| {
            is_text(&ModelCard {
                id: id.to_owned(),
                display_name: String::new(),
                description: String::new(),
                input_token_limit: None,
                output_token_limit: None,
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
            })
        };
        // Generic servers load embedding, reranking, and speech models
        // behind the same listing as the chat models.
        assert!(!text("text-embedding-3-large"));
        assert!(!text("bge-reranker-v2-m3"));
        assert!(!text("Systran/faster-whisper-large-v3"));
        assert!(text("gpt-4o-mini"));
        assert!(text("qwen2.5-coder-7b-instruct"));
    }

    #[test]
    fn a_page_without_a_data_array_yields_no_models() {
        let models = normalize_models(&serde_json::json!({ "error": "nope" }));
        assert!(models.is_empty());
    }

    #[test]
    fn the_balance_reads_the_first_currency_entry() {
        let balance: WireBalance = serde_json::from_str(
            r#"{
                "is_available": true,
                "balance_infos": [
                    { "currency": "USD", "total_balance": "110.50", "granted_balance": "5.00" }
                ]
            }"#,
        )
        .expect("balance parses");
        assert_eq!(
            allowance(&balance),
            Some(Allowance {
                amount: "$110.50".to_owned(),
                kind: String::new(),
            })
        );

        // Another currency keeps its code beside the figure.
        let cny: WireBalance =
            serde_json::from_str(r#"{"balance_infos":[{"currency":"CNY","total_balance":"8"}]}"#)
                .expect("parses");
        assert_eq!(allowance(&cny).unwrap().amount, "¥8.00".to_owned());

        // No entries, or an unparseable figure: no allowance.
        let empty: WireBalance = serde_json::from_str(r#"{"balance_infos":[]}"#).expect("parses");
        assert_eq!(allowance(&empty), None);
    }
}
