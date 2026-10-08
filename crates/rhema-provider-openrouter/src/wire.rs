//! OpenRouter wire types and the pure mapping between them and rhema-core's
//! model. No I/O here — [`crate::provider`] drives these functions.
//!
//! OpenRouter aggregates hosted models behind an OpenAI-compatible
//! chat-completions API; the dialect is the same the llama.cpp adapter
//! speaks, with OpenRouter's documented deviations noted. Mapping
//! constraints (the wire has a role for everything rhema-core has):
//! - `model` is a `vendor/model` slug (e.g. `openai/gpt-4o-mini`), passed
//!   through verbatim and never validated here: OpenRouter owns rejection,
//!   so a malformed request surfaces as a 400 mapped to
//!   [`ProviderError::Api`](rhema_core::ProviderError::Api).
//! - Roles map one-to-one. `ModelRequest.system` becomes a leading `system`
//!   message; each `Role::System` message becomes its own `system` message
//!   (text blocks joined by a blank line). `Role::User` and `Role::Assistant`
//!   map to their like-named roles; text blocks of one message are joined by
//!   a blank line into the single `content` string.
//! - An assistant turn with text and/or tool calls becomes ONE wire message:
//!   `content` is omitted when there is no text, `tool_calls` when there are
//!   no calls. OpenRouter rejects assistant messages with neither, so an
//!   empty assistant turn is dropped from history.
//! - Tool results become `{role: "tool", tool_call_id, content}` messages,
//!   one per result block, matched by id — whichever rhema message carries
//!   them. `ToolResult::is_error` has no wire representation; the content
//!   text alone must tell the model the call failed.
//! - `ToolCall::provider_data` is ignored: OpenRouter call ids are plain
//!   strings that round-trip on the wire, so there is nothing opaque to
//!   carry.
//! - Tool specs map to `[{type: "function", function: {name, description,
//!   parameters}}]`, with `parameters` passed through verbatim; OpenRouter
//!   wants the tools on every request so it can validate the schema.
//! - Every call streams (`stream: true`). `stream_options` is deliberately
//!   NOT sent: OpenRouter deprecates the field ("no effect") and always
//!   includes full usage on the stream's final chunk.
//! - Streaming chunks: `delta.content` becomes one `TextDelta` per
//!   non-empty piece. `delta.tool_calls[]` fragments are keyed by `index`;
//!   the FIRST fragment of an index emits `ToolCallStart` (the wire id, or a
//!   synthesized `call_{index}` when absent) and every non-empty
//!   `function.arguments` fragment emits `ToolCallArgsDelta`. OpenRouter
//!   sends id and name on the first fragment and argument text in pieces.
//! - The usage chunk — always the last before `data: [DONE]` — deviates
//!   from OpenAI's: instead of emptying `choices` it repeats one
//!   content-free choice carrying the same `finish_reason` plus a
//!   `native_finish_reason` this crate ignores. Last-value-wins turns it
//!   into a pure accounting frame, not a second terminal event.
//! - The assembled message lists text parts in stream order, then tool
//!   calls in ascending index order. A call's arguments are the
//!   concatenated fragments parsed as JSON; an unparseable payload wraps as
//!   `{"_openrouter_args_text": <raw>}` — a malformed call must not fail
//!   the turn. Empty arguments become `{}`.
//! - `finish_reason` maps onto rhema-core's stop reason, with any tool call
//!   in the assembled message winning as
//!   [`StopReason::ToolUse`](rhema_core::StopReason::ToolUse) — the same
//!   rule as the sibling adapters.
//! - Models: `GET /models` needs no auth in practice and paginates with
//!   `limit`/`offset` (`links.next` is null on the last page). An entry's
//!   `pricing.prompt`/`pricing.completion` are STRINGS of USD per single
//!   token; they are converted to per-MTok f64s, with absent, unparseable,
//!   or negative ("-1"-style sentinel) values meaning "not priced".

use std::collections::BTreeMap;

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::{ModelRequest, ThinkingEffort};
use rhema_core::tool::ToolCall;
use rhema_provider_google::{GeminiModel, Pricing};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker key wrapping an unparseable streamed arguments payload so the
/// call still round-trips through history.
const WRAPPED_ARGS_KEY: &str = "_openrouter_args_text";

// --- Request wire types (serialize) ---

/// The `POST /chat/completions` request body. Always streaming; usage needs
/// no opt-in on OpenRouter (see the module notes on `stream_options`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatCompletionRequest {
    /// Model slug, passed through verbatim.
    pub model: String,
    /// Conversation turns.
    pub messages: Vec<WireMessage>,
    /// Tools offered this step, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    /// Always `true`: the provider only speaks the streaming dialect.
    pub stream: bool,
    /// Sampling temperature, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output token ceiling, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// How much reasoning the model should do, when the driver named a
    /// level; absent means the routed model's default stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<WireReasoning>,
}

/// The unified `reasoning` request field: OpenRouter normalizes every
/// provider's thinking controls onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WireReasoning {
    /// How much reasoning the model should do.
    pub effort: &'static str,
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
    /// from history: interleaved-thinking models continue their chain of
    /// thought across tool calls instead of re-deriving it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
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

/// One streamed `chat.completion.chunk`, or the error object OpenRouter
/// sends in place of one when the call fails after the stream committed.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ChatCompletionChunk {
    /// Chunk deltas; the content-free choice of the usage chunk carries no
    /// payload.
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    /// Token accounting; always on the final chunk.
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// OpenRouter's error detail, on a stream aborted by a failure: the
    /// chunk body carries the `{"error": {...}}` wrapper, so this field
    /// holds the detail directly.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// One choice of a chunk; exactly one choice per content chunk, one
/// content-free repeated choice on the usage chunk, none expected
/// otherwise.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireChoice {
    /// The piece generated since the last chunk, when any.
    #[serde(default)]
    pub delta: Option<WireDelta>,
    /// Why generation stopped; `null` on every chunk but the last content
    /// one (and repeated verbatim on the usage chunk).
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The `delta` object of a [`WireChoice`]. Fields beyond `content`,
/// `tool_calls`, and `reasoning` are ignored.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireDelta {
    /// A piece of assistant text.
    #[serde(default)]
    pub content: Option<String>,
    /// A piece of the routed model's reasoning trace, normalized by
    /// OpenRouter across providers. Emitted as [`Event::ThinkingDelta`],
    /// never assembled into the message.
    #[serde(default)]
    pub reasoning: Option<String>,
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

/// The `usage` object of the final chunk. OpenRouter may add its own
/// accounting fields (`cost`, reasoning tokens, ...); everything unmapped
/// is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WireUsage {
    /// Prompt tokens billed.
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    /// Completion tokens billed.
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    /// Prompt cache accounting, when the routed provider reports it.
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

/// The `prompt_tokens_details` object of a [`WireUsage`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct WirePromptTokensDetails {
    /// Prompt subset served from the routed provider's prompt cache.
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// The `error` object of a non-2xx response body, or of a mid-stream error
/// chunk: `{"error": {"code": ..., "message": ..., "metadata": ...}}`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireErrorBody {
    /// The error detail, when the body has the documented shape.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// OpenRouter error detail. The `code` is a number on HTTP errors but a
/// string (e.g. `"server_error"`) on mid-stream ones, so it is kept
/// uninterpreted; only `message` is ever surfaced.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct WireErrorDetail {
    /// Number or string error code, per the shape above.
    #[serde(default)]
    pub code: Option<Value>,
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

/// One page of the `GET /models` response.
#[derive(Debug, Default, Deserialize)]
pub struct WireModelsPage {
    /// The models on this page.
    #[serde(default)]
    pub data: Vec<WireModel>,
    /// Paging links; `next` is null on the last page.
    #[serde(default)]
    pub links: Option<WirePageLinks>,
}

/// The `links` object of a [`WireModelsPage`].
#[derive(Debug, Default, Deserialize)]
pub struct WirePageLinks {
    /// Path of the next page, when there is one.
    #[serde(default)]
    pub next: Option<String>,
}

/// One wire model object. Dozens of catalog fields exist; only the ones a
/// picker row shows are kept.
#[derive(Debug, Default, Deserialize)]
pub struct WireModel {
    /// Model slug (e.g. `openai/gpt-4o-mini`), usable as the request's
    /// `model` value.
    #[serde(default)]
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// What the model is for.
    #[serde(default)]
    pub description: String,
    /// Max context in tokens, when advertised.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Prices in USD per single token, when advertised.
    #[serde(default)]
    pub pricing: Option<WirePricing>,
}

/// The `pricing` object of a [`WireModel`]; the per-request, image, and
/// audio prices are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct WirePricing {
    /// USD per prompt token, as a decimal string.
    #[serde(default)]
    pub prompt: Option<String>,
    /// USD per completion token, as a decimal string.
    #[serde(default)]
    pub completion: Option<String>,
    /// USD per prompt token served from the provider's cache, when the
    /// upstream supports prompt caching.
    #[serde(default)]
    pub input_cache_read: Option<String>,
}

impl WireModelsPage {
    /// Whether another page follows: `links.next` present.
    pub fn next_page(&self) -> bool {
        self.links
            .as_ref()
            .is_some_and(|links| links.next.is_some())
    }
}

/// A USD-per-token string as a plain number; absent, unparseable, and
/// negative (sentinel) values all mean "not priced".
fn token_price(raw: Option<&str>) -> Option<f64> {
    let value: f64 = raw?.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// The model's per-MTok price, when its entry prices both directions.
/// The catalog's cache-read rate rides along when published.
fn per_mtok_pricing(wire: &WireModel) -> Option<Pricing> {
    let pricing = wire.pricing.as_ref()?;
    let cached_input_per_mtok =
        token_price(pricing.input_cache_read.as_deref()).map(|rate| rate * 1_000_000.0);
    Some(Pricing {
        input_per_mtok: token_price(pricing.prompt.as_deref())? * 1_000_000.0,
        output_per_mtok: token_price(pricing.completion.as_deref())? * 1_000_000.0,
        cached_input_per_mtok,
    })
}

impl From<WireModel> for GeminiModel {
    fn from(wire: WireModel) -> Self {
        // Priced before the fields move out.
        let pricing = per_mtok_pricing(&wire);
        Self {
            id: wire.id,
            display_name: wire.name,
            description: wire.description,
            input_token_limit: wire.context_length,
            // OpenRouter shares the context window between directions and
            // reports no output ceiling.
            output_token_limit: None,
            // Every listed model is offered through chat completions.
            generation_methods: vec!["generateContent".to_owned()],
            pricing,
        }
    }
}

// --- Mapping: ModelRequest -> wire ---

/// Builds the chat-completions request body for `req`. The model is passed
/// through verbatim and nothing is validated: OpenRouter owns rejection.
pub fn request_body(req: &ModelRequest) -> ChatCompletionRequest {
    let mut messages: Vec<WireMessage> = Vec::new();
    if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
        messages.push(WireMessage {
            role: "system".into(),
            reasoning: None,
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
                        reasoning: None,
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
                        reasoning: None,
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
                // The API rejects an assistant message with neither content
                // nor tool_calls.
                if !text.is_empty() || !calls.is_empty() {
                    messages.push(WireMessage {
                        role: "assistant".into(),
                        content: (!text.is_empty()).then_some(text),
                        tool_calls: (!calls.is_empty()).then_some(calls),
                        reasoning: (!reasoning.is_empty()).then_some(reasoning),
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
        temperature: req.params.temperature,
        max_tokens: req.params.max_tokens,
        reasoning: reasoning_effort(req.params.thinking),
    }
}

/// OpenRouter's `reasoning.effort` spelling of a thinking level; `None`
/// sends nothing and the routed model's default stands.
fn reasoning_effort(thinking: ThinkingEffort) -> Option<WireReasoning> {
    let effort = match thinking {
        ThinkingEffort::Default => return None,
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
    };
    Some(WireReasoning { effort })
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
            reasoning: None,
            content: Some(result.content.clone()),
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
/// `finish_reason` and `usage` may arrive on separate chunks (OpenRouter
/// sends a content-free choice with the reason, then the usage chunk
/// repeating it); the last value seen wins. When the assembled message
/// contains tool calls, the effective stop reason is
/// [`StopReason::ToolUse`] regardless of the wire value.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    texts: Vec<String>,
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
    /// [`ProviderError::Api`] when the chunk is OpenRouter's mid-stream
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
            if let Some(text) = delta.reasoning.filter(|text| !text.is_empty()) {
                sink.emit(Event::ThinkingDelta(&text));
                self.reasoning.push_str(&text);
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                sink.emit(Event::TextDelta(&text));
                self.texts.push(text);
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
    /// `Role::Assistant`: text parts in stream order, then tool calls in
    /// ascending index order; an empty stream yields an empty content list,
    /// not an error.
    pub fn finish(self) -> ModelOutput {
        let mut content: Vec<Content> = self.texts.into_iter().map(Content::Text).collect();
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
    fn a_named_thinking_effort_rides_in_reasoning_effort() {
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: "z-ai/glm-5.3".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::High,
            },
        };
        let body = serde_json::to_string(&request_body(&req)).unwrap();
        assert!(
            body.contains("\"reasoning\":{\"effort\":\"high\"}"),
            "{body}"
        );

        // Default sends nothing: the routed model's default stands.
        req.params.thinking = ThinkingEffort::Default;
        let body = serde_json::to_string(&request_body(&req)).unwrap();
        assert!(!body.contains("\"reasoning\""), "{body}");
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
                model: "z-ai/glm-5.3".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::Default,
            },
        };
        let body = serde_json::to_value(request_body(&req)).expect("body serializes");
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning"], reasoning);

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
                .all(|m| m.get("reasoning").is_none()),
            "{body}"
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
                model: "qwen/qwen3-coder".into(),
                temperature: Some(0.2),
                max_tokens: Some(256),
            },
        };

        let json = serde_json::to_value(request_body(&req)).expect("body serializes");

        assert_eq!(json["model"], "qwen/qwen3-coder");
        assert_eq!(json["stream"], json!(true));
        // Usage is unconditional on OpenRouter; the deprecated opt-in is
        // deliberately absent.
        assert!(json.get("stream_options").is_none());
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
            params: ChatParams::new("openai/gpt-4o-mini"),
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
            params: ChatParams::new("openai/gpt-4o-mini"),
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
            params: ChatParams::new("openai/gpt-4o-mini"),
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
                    "{\"id\":\"gen-1\",\"provider\":\"openai\",",
                    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\",\"role\":\"assistant\"},",
                    "\"finish_reason\":null}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"content\":\"lo\"},",
                    "\"finish_reason\":\"stop\",\"native_finish_reason\":\"stop\"}]}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        // OpenRouter's usage chunk: one content-free choice repeating the
        // finish reason, then the usage object.
        assembler
            .push(
                chunk(concat!(
                    "{\"choices\":[{\"delta\":{\"content\":\"\",\"role\":\"assistant\"},",
                    "\"finish_reason\":\"stop\",\"native_finish_reason\":\"stop\"}],",
                    "\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12,",
                    "\"prompt_tokens_details\":{\"cached_tokens\":4},\"cost\":0.0001}}"
                )),
                &mut sink,
            )
            .expect("chunk feeds");
        let out = assembler.finish();

        assert_eq!(
            out.message,
            Message {
                role: Role::Assistant,
                content: vec![Content::Text("Hel".into()), Content::Text("lo".into())],
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
                args: json!({ "_openrouter_args_text": "not json" }),
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
    fn reasoning_before_a_tool_call_is_kept_for_replay() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        for payload in [
            r#"{"choices":[{"delta":{"reasoning":"I should list "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"the files first"}}]}"#,
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
            out.message.content[0],
            Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "a".into(),
                args: json!({}),
                provider_data: json!({ "reasoning": "I should list the files first" }),
            })
        );
    }

    #[test]
    fn a_tool_call_beats_the_wire_finish_reason() {
        // Routed providers disagree on the reason for tool-call turns.
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
            ("error", StopReason::Other("error".into())),
            (
                "model_call_error",
                StopReason::Other("model_call_error".into()),
            ),
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
    fn empty_text_deltas_are_ignored() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk("{\"choices\":[{\"delta\":{\"content\":\"\",\"role\":\"assistant\"}}]}"),
                &mut sink,
            )
            .expect("chunk feeds");
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
        assert_eq!(sink.0, vec!["TextDelta(\"ok\")".to_owned()]);
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
        // The documented shape: string code, message, finish_reason "error".
        let err = assembler
            .push(
                chunk(concat!(
                    "{\"id\":\"cmpl-1\",\"error\":{\"code\":\"server_error\",",
                    "\"message\":\"Provider disconnected unexpectedly\"},",
                    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},",
                    "\"finish_reason\":\"error\"}]}"
                )),
                &mut sink,
            )
            .expect_err("mid-stream error is an error");
        assert_eq!(
            err,
            ProviderError::Api("Provider disconnected unexpectedly".into())
        );
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
        // HTTP-phase errors carry numeric codes.
        let body: WireErrorBody = serde_json::from_str(
            "{\"error\":{\"code\":401,\"message\":\"No auth credentials found\"}}",
        )
        .expect("error body parses");
        assert_eq!(body.message("raw"), "No auth credentials found");

        let unshaped: WireErrorBody = serde_json::from_str("not even json").unwrap_or_default();
        assert_eq!(unshaped.message("not even json"), "not even json");
    }

    #[test]
    fn model_entries_map_to_gemini_rows_with_per_mtok_pricing() {
        let page: WireModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {
                        "id": "openai/gpt-4o-mini",
                        "name": "GPT-4o mini",
                        "description": "the small openai model",
                        "context_length": 128000,
                        "architecture": { "modality": "text->text" },
                        "pricing": {
                            "prompt": "0.00000015",
                            "completion": "0.0000006",
                            "request": "0",
                            "input_cache_read": "0.0000000375"
                        },
                        "supported_parameters": ["tools", "temperature"]
                    },
                    {
                        "id": "some/free-model",
                        "name": "Free",
                        "pricing": { "prompt": "0", "completion": "0" }
                    },
                    {
                        "id": "some/unpriced-model",
                        "name": "Unpriced"
                    }
                ],
                "links": { "next": null }
            }"#,
        )
        .expect("models page parses");

        assert!(!page.next_page());
        let models: Vec<GeminiModel> = page.data.into_iter().map(Into::into).collect();

        assert_eq!(models[0].id, "openai/gpt-4o-mini");
        assert_eq!(models[0].display_name, "GPT-4o mini");
        assert_eq!(models[0].description, "the small openai model");
        // Per-token strings scale by 1M: 0.00000015 USD/token = 1.5 USD/MTok.
        assert_eq!(models[0].input_token_limit, Some(128_000));
        assert_eq!(models[0].output_token_limit, None);
        assert_eq!(models[0].generation_methods, vec!["generateContent"]);
        assert_eq!(
            models[0].pricing,
            Some(Pricing {
                input_per_mtok: 0.15,
                output_per_mtok: 0.6,
                // 0.0000000375 USD/token scales to 0.0375 USD/MTok.
                cached_input_per_mtok: Some(0.0375),
            })
        );

        // Free is a price: zero, not absence.
        assert_eq!(
            models[1].pricing,
            Some(Pricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
            })
        );
        // No pricing object means no price at all.
        assert_eq!(models[2].pricing, None);
    }

    #[test]
    fn unparseable_and_sentinel_prices_mean_not_priced() {
        let page: WireModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {
                        "id": "a/b",
                        "pricing": { "prompt": "-1", "completion": "0.000001" }
                    },
                    {
                        "id": "c/d",
                        "pricing": { "prompt": "free", "completion": "0.000001" }
                    },
                    {
                        "id": "e/f",
                        "pricing": { "prompt": "0.000001" }
                    }
                ]
            }"#,
        )
        .expect("models page parses");
        let models: Vec<GeminiModel> = page.data.into_iter().map(Into::into).collect();
        for model in models {
            assert_eq!(model.pricing, None, "for {}", model.id);
        }
    }

    #[test]
    fn pages_link_the_next_page_only_when_one_exists() {
        let more: WireModelsPage =
            serde_json::from_str("{\"data\":[],\"links\":{\"next\":\"/api/v1/models?offset=1\"}}")
                .unwrap();
        assert!(more.next_page());

        let last: WireModelsPage = serde_json::from_str("{\"data\":[]}").unwrap();
        assert!(!last.next_page());
    }
}
