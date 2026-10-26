//! OpenAI-compatible wire types and the pure mapping between them and
//! rhema-core's model. No I/O here — [`crate::provider`] drives these
//! functions. The server is llama.cpp's `llama-server`; other OpenAI-shaped
//! servers close enough to its dialect work too, but nothing is claimed
//! beyond what llama.cpp does.
//!
//! Mapping constraints (the wire has a role for everything rhema-core has):
//! - `model` is passed through verbatim — llama.cpp serves one loaded model
//!   and ignores the field, but OpenAI-shaped servers require it. Unlike the
//!   Gemini adapter, no request validation happens here; a malformed request
//!   surfaces as a 400 from the server, mapped to
//!   [`ProviderError::Api`](rhema_core::ProviderError::Api).
//! - Roles map one-to-one. `ModelRequest.system` becomes a leading `system`
//!   message; each `Role::System` message becomes its own `system` message
//!   (text blocks joined by a blank line). `Role::User` and `Role::Assistant`
//!   map to their like-named roles; text blocks of one message are joined by
//!   a blank line into the single `content` string.
//! - An assistant turn with text and/or tool calls becomes ONE wire message:
//!   `content` is omitted when there is no text, `tool_calls` when there are
//!   no calls. The server rejects assistant messages with neither, so an
//!   empty assistant turn is dropped from history.
//! - Tool results become `{role: "tool", tool_call_id, content}` messages,
//!   one per result block, matched by id — whichever rhema message carries
//!   them. `ToolResult::is_error` has no wire representation; the content
//!   text alone must tell the model the call failed.
//! - `ToolCall::provider_data` is ignored: llama.cpp call ids are plain
//!   strings that round-trip on the wire, so there is nothing opaque to
//!   carry (unlike Gemini's thought signatures).
//! - Tool specs map to `[{type: "function", function: {name, description,
//!   parameters}}]`, with `parameters` passed through verbatim.
//! - Every call streams: `stream: true` plus
//!   `stream_options: {include_usage: true}` — without the flag llama.cpp
//!   sends no usage at all in a streamed response.
//! - Streaming chunks: `delta.content` becomes one `TextDelta` per
//!   non-empty piece. `delta.tool_calls[]` fragments are keyed by `index`;
//!   the FIRST fragment of an index emits `ToolCallStart` (the wire id, or a
//!   synthesized `call_{index}` when absent) and every non-empty
//!   `function.arguments` fragment emits `ToolCallArgsDelta`. llama.cpp
//!   sends id and name on the first fragment and argument text in pieces.
//! - The assembled message carries the streamed text as one part — chunk
//!   boundaries are transport artifacts, and per-chunk parts would be
//!   replayed next request joined by blank lines, teaching the model to
//!   emit `token\n\ntoken` prose — then tool calls
//!   in ascending index order. A call's arguments are the concatenated
//!   fragments parsed as JSON; an unparseable payload wraps as
//!   `{"_llama_args_text": <raw>}` — a malformed call must not fail the
//!   turn. Empty arguments become `{}`.
//! - `finish_reason` maps onto rhema-core's stop reason, with any tool call
//!   in the assembled message winning as
//!   [`StopReason::ToolUse`](rhema_core::StopReason::ToolUse) — the same
//!   rule as the Gemini adapter.
//! - `delta.reasoning_content` (thinking models) becomes one
//!   `ThinkingDelta` per non-empty piece, like Gemini thought parts.
//! - The thinking effort is a server-side knob on llama.cpp (a launch
//!   flag, not a request field): no effort parameter is ever sent.
//! - Errors: a non-2xx body is `{"error": {code, message, type}}`; the
//!   mapper keeps `error.message` and falls back to the raw body. The
//!   server can also fail mid-stream with a `data: {"error": ...}` chunk
//!   (then closing the stream), which surfaces as the same
//!   [`ProviderError::Api`](rhema_core::ProviderError::Api).

use std::collections::BTreeMap;

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::ModelRequest;
use rhema_core::tool::ToolCall;
use rhema_provider::ModelCard;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker key wrapping an unparseable streamed arguments payload so the
/// call still round-trips through history.
const WRAPPED_ARGS_KEY: &str = "_llama_args_text";

// --- Request wire types (serialize) ---

/// The `POST /v1/chat/completions` request body. Always streaming.
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
    /// Always requests the usage-only final chunk; without it llama.cpp
    /// sends no token accounting on a streamed call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<WireStreamOptions>,
    /// Sampling temperature, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
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

/// The `stream_options` request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireStreamOptions {
    /// Ask for the usage-only final chunk.
    pub include_usage: bool,
}

// --- Response wire types (deserialize) ---

/// One streamed `chat.completion.chunk`, or the error object the server
/// sends in place of one when the call fails mid-stream.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ChatCompletionChunk {
    /// Chunk deltas; empty on the usage-only final chunk.
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    /// Token accounting; only on the final chunk, and only when the request
    /// asked for it.
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// The server's error detail, on a stream aborted by a failure: the
    /// chunk body is the whole `{"error": {code, message, type}}` wrapper,
    /// so this field holds the detail directly.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// One choice of a chunk; llama.cpp always sends exactly one, the usage
/// chunk sends none.
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
/// `tool_calls`, and `reasoning_content` are ignored.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireDelta {
    /// A piece of assistant text.
    #[serde(default)]
    pub content: Option<String>,
    /// A piece of the model's reasoning trace, as the server emits it for
    /// thinking models. Emitted as [`Event::ThinkingDelta`], never
    /// assembled into the message.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Tool-call fragments, keyed by their `index`.
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallDelta>>,
}

/// One tool-call fragment of a [`WireDelta`]. The first fragment of an
/// index carries `id` and the function `name`; continuation fragments carry
/// only argument text.
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

/// llama.cpp error detail.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireErrorDetail {
    /// HTTP-ish status code.
    #[serde(default)]
    pub code: Option<i32>,
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

/// One page of the `GET /v1/models` response.
#[derive(Debug, Default, Deserialize)]
pub struct WireModelsPage {
    /// The models the server can serve (llama.cpp: the one loaded model).
    #[serde(default)]
    pub data: Vec<WireModel>,
}

/// A wire model object; only the id is rhema-relevant.
#[derive(Debug, Default, Deserialize)]
pub struct WireModel {
    /// Model identifier, usable as the request's `model` value.
    #[serde(default)]
    pub id: String,
}

impl From<LlamaModel> for ModelCard {
    fn from(model: LlamaModel) -> Self {
        Self {
            display_name: model.id.clone(),
            id: model.id,
            description: String::new(),
            input_token_limit: model.context_length,
            // The listing reports one shared context window and no
            // per-direction output ceiling.
            output_token_limit: None,
            // Every listed model is offered through chat completions.
            supports_vision: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }
}

/// One model from the list, normalized: id plus whatever context length
/// and load state the server reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaModel {
    /// Model identifier, usable as the request's `model` value.
    pub id: String,
    /// Context window the server reported, when it did.
    pub context_length: Option<u64>,
    /// Whether the server says the model is loaded (router mode);
    /// single-model servers leave this `None`.
    pub loaded: Option<bool>,
}

/// Scans one model entry for a context length. The listing shape drifted
/// across llama.cpp versions and modes, so this tolerates every field a
/// server has been seen to use: top-level `n_ctx` / `context_length` /
/// `context_window`, a `meta.n_ctx_train` object, and router entries whose
/// `loaded_info` merge carries `n_ctx`.
fn context_length_of(entry: &serde_json::Value) -> Option<u64> {
    let n_ctx = |value: &serde_json::Value| value.as_u64();
    entry
        .get("n_ctx")
        .and_then(n_ctx)
        .or_else(|| entry.get("context_length").and_then(n_ctx))
        .or_else(|| entry.get("context_window").and_then(n_ctx))
        .or_else(|| {
            entry
                .get("meta")
                .and_then(|meta| meta.get("n_ctx_train"))
                .and_then(n_ctx)
        })
        .or_else(|| {
            entry
                .get("loaded_info")
                .and_then(|info| info.get("n_ctx"))
                .and_then(n_ctx)
        })
}

/// Normalizes the models list: keeps entries that carry an id, resolves
/// their context lengths, and records whether the server says the model is
/// loaded (router mode; single-model servers leave this `None`).
pub fn normalize_models(page: &serde_json::Value) -> Vec<LlamaModel> {
    let entries = page
        .get("data")
        .or_else(|| page.get("objects"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let loaded = entry
                .pointer("/status/value")
                .and_then(|value| value.as_str())
                .map(|status| status == "loaded");
            Some(LlamaModel {
                id,
                context_length: context_length_of(entry),
                loaded,
            })
        })
        .collect()
}

/// Extracts the context window from a `/props` response: top-level
/// `n_ctx`, or the one nested in `default_generation_settings`.
pub fn context_from_props(props: &serde_json::Value) -> Option<u64> {
    props
        .get("n_ctx")
        .and_then(|value| value.as_u64())
        .filter(|n_ctx| *n_ctx > 0)
        .or_else(|| {
            props
                .pointer("/default_generation_settings/n_ctx")
                .and_then(|value| value.as_u64())
                .filter(|n_ctx| *n_ctx > 0)
        })
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
                // The server rejects an assistant message with neither
                // content nor tool_calls.
                if !text.is_empty() || !calls.is_empty() {
                    messages.push(WireMessage {
                        role: "assistant".into(),
                        content: (!text.is_empty()).then_some(text),
                        tool_calls: (!calls.is_empty()).then_some(calls),
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
        stream_options: Some(WireStreamOptions {
            include_usage: true,
        }),
        temperature: req.params.temperature,
        max_tokens: req.params.max_tokens,
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
            tool_calls: None,
            tool_call_id: Some(result.id.clone()),
        }),
        _ => None,
    })
}

/// Call arguments as their JSON text; a `null` payload becomes `{}`
/// (meaningless on this wire, and the server's grammar would reject it).
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
}

/// Assembles streamed chunks into the final [`ModelOutput`], emitting
/// streaming events as deltas arrive.
///
/// `finish_reason` and `usage` may arrive on separate chunks (llama.cpp
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
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Api`] (no status) when the chunk is
    /// the server's mid-stream error.
    pub fn push(
        &mut self,
        chunk: ChatCompletionChunk,
        sink: &mut dyn EventSink,
    ) -> Result<(), ProviderError> {
        if let Some(error) = &chunk.error {
            return Err(ProviderError::Api {
                message: error
                    .message
                    .clone()
                    .unwrap_or_else(|| "mid-stream error".to_owned()),
                status: None,
            });
        }
        if let Some(usage) = chunk.usage {
            self.usage = Usage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
                cached_input_tokens: usage
                    .prompt_tokens_details
                    .and_then(|details| details.cached_tokens)
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
        let partial = self.calls.entry(index).or_default();
        if !partial.started {
            partial.started = true;
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
        }
        if let Some(piece) = fragment
            .function
            .and_then(|function| function.arguments)
            .filter(|piece| !piece.is_empty())
        {
            // A stray fragment with no start behind it synthesizes the
            // id the start path would have used.
            let id = partial
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{index}"));
            sink.emit(Event::ToolCallArgsDelta {
                id: &id,
                delta: &piece,
            });
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
            content.push(Content::ToolCall(ToolCall {
                id: partial.id.unwrap_or_else(|| format!("call_{index}")),
                name: partial.name.unwrap_or_default(),
                args: parse_args(&partial.arguments),
                provider_data: Value::Null,
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
/// rule as the Gemini adapter.
fn stop_reason(finish_reason: Option<&str>, has_calls: bool) -> StopReason {
    if has_calls {
        return StopReason::ToolUse;
    }
    match finish_reason {
        None | Some("") | Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
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
                model: "qwen2.5-coder-7b".into(),
                temperature: Some(0.2),
                max_tokens: Some(256),
                thinking: rhema_core::request::ThinkingEffort::Default,
            },
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");

        assert_eq!(json["model"], "qwen2.5-coder-7b");
        assert_eq!(json["stream"], json!(true));
        assert_eq!(json["stream_options"]["include_usage"], json!(true));
        assert_eq!(json["temperature"], json!(0.2_f32));
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
            params: ChatParams::new("qwen2.5-coder-7b"),
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
            params: ChatParams::new("qwen2.5-coder-7b"),
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
            params: ChatParams::new("qwen2.5-coder-7b"),
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
                chunk("{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk("{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
        // llama.cpp's usage-only final chunk: no choices at all.
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
            params: ChatParams::new("qwen2.5-coder-7b"),
        };
        let json = serde_json::to_value(request_body(&req)).expect("body serializes");
        let messages = json["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], "to re -add");
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
                "ToolCallArgsDelta { id: \"call_abc\", delta: \"{\\\"x\\\":\" }".to_owned(),
                "ToolCallArgsDelta { id: \"call_abc\", delta: \"1}\" }".to_owned(),
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
                args: json!({ "_llama_args_text": "not json" }),
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
        // llama.cpp reports "stop" for parsed tool-call turns too.
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
        assert_eq!(
            err,
            ProviderError::Api {
                message: "inference failed".into(),
                status: None,
            }
        );
        assert!(sink.0.is_empty());

        // An unshaped error body falls back to the fixed raw text.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(chunk("{\"error\":{}}"), &mut sink)
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
    fn models_page_yields_the_ids() {
        let page: WireModelsPage = serde_json::from_str(
            r#"{
                "object": "list",
                "data": [
                    { "id": "qwen2.5-coder-7b", "object": "model", "owned_by": "llamacpp" }
                ]
            }"#,
        )
        .expect("models page parses");
        let ids: Vec<String> = page.data.into_iter().map(|model| model.id).collect();
        assert_eq!(ids, vec!["qwen2.5-coder-7b".to_owned()]);
    }
}
