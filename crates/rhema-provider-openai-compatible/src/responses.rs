//! The Responses-API wire: `POST {base}/responses`, the shape OpenAI
//! introduced beside chat completions and the one DeepSeek, OpenCode's
//! gateway, and llama.cpp's server now also speak.
//!
//! Why a second wire exists here: chat-completions tool messages carry
//! text only, so a tool result's images need a follow-up user turn to
//! arrive. The Responses API takes content parts in a
//! `function_call_output` itself, an `input_image` part rides the
//! result it came from, with no reroute, which is the honest wire home
//! for `see`'s staged images on the servers that support it.
//!
//! The API is stateless where rhema points it (DeepSeek publishes
//! `previous_response_id`/`store` as unsupported): every request
//! replays the whole conversation as `input` items, the same
//! full-replay rule every other adapter follows, and `store: false`
//! says the caller keeps no server-side copy.
//!
//! Request mapping, mirroring the chat dialect's choices:
//!
//! - `ModelRequest.normalized` has already folded `ModelRequest.system`
//!   and every `Role::System` message into one string, which lands in
//!   `instructions`; the API inserts it as the first system message.
//! - `ChatParams::top_p` maps onto `top_p`; `stop` has no field in the
//!   Responses request shape and is ignored.
//! - The dialect's request-root prompt-cache opt-in
//!   ([`Dialect::automatic_cache_control`]) rides as `cache_control`
//!   exactly as in chat completions; the API documents the field at the
//!   request root here too. So does the conversation id
//!   ([`Dialect::sends_session_id`]) as `session_id`.
//! - A text-only turn keeps a bare string content, strict servers
//!   predate the parts shape; a turn with images spells a parts array
//!   of `input_text`/`input_image` items, the data-URI form a pasted
//!   image already rides.
//! - An assistant turn replays as a `reasoning` item (the chain of
//!   thought that led to its calls, stored on the calls'
//!   `provider_data` on the way in, sent as the canonical `summary`
//!   array), then the text message, then one `function_call` item per
//!   call.
//! - A tool result replays as a `function_call_output`: bare string
//!   output for text-only results, a parts array, text first, then
//!   each image, when the result carries images.
//!
//! The stream is a sequence of typed server-sent events, each
//! carrying its `type` in the JSON payload, ending with
//! `response.completed`/`response.incomplete`/`response.failed`,
//! there is no `[DONE]` sentinel (one arriving from a proxy is
//! tolerated). [`ResponsesAssembler`] folds the events into the same
//! streaming contract as the chat [`crate::wire::ChunkAssembler`]:
//! `ThinkingDelta`/`TextDelta` as they stream, a call announced by its
//! item's `added` event with argument pieces as deltas, and usage from
//! the terminal event. Event types the assembler does not know are
//! ignored, so servers free to grow the vocabulary stay compatible.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::{ModelRequest, ThinkingEffort};
use rhema_core::tool::ToolCall;

use crate::wire::{
    Dialect, OPENAI_COMPATIBLE, arguments_text, joined_texts, parse_args, reasoning_effort_value,
};

// --- Request wire types (serialize) ---

/// The `POST {base}/responses` request body. Streaming only, stateless
/// by the `store: false` rhema always sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest {
    /// Model identifier, passed through verbatim.
    pub model: String,
    /// The system prompt: the normalized `ModelRequest.system`, with every
    /// `Role::System` message already folded in. Inserted by the API as
    /// the first system message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The conversation so far, as input items in order.
    pub input: Vec<WireInputItem>,
    /// Tools offered this step, when any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireResponsesTool>,
    /// Always `true`: the provider only speaks the streaming dialect.
    pub stream: bool,
    /// Always `false`: rhema replays its own history every turn and
    /// keeps no server-side response copy.
    pub store: bool,
    /// Sampling temperature, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus-sampling threshold, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Output token ceiling, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// The request-root prompt-cache opt-in the dialect's fn named for
    /// this model, when one exists
    /// ([`Dialect::automatic_cache_control`]) — the same directive the
    /// chat-completions body carries, this API documents it at the
    /// request root too. Absent, the vendor caches, or not, by its own
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
    /// The request's conversation id, when the dialect forwards it and
    /// the driver named one ([`Dialect::sends_session_id`]) — the API
    /// documents the field at the request root here too. Absent, the
    /// server derives whatever it needs from the messages alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The cache-routing key, when the dialect takes it there
    /// ([`Dialect::sends_prompt_cache_key`] at `Both`): the
    /// conversation id again, under the OpenAI spelling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// The thinking level, when the request sets a non-default one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<WireReasoning>,
}

/// One conversation item of [`ResponsesRequest::input`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireInputItem {
    /// One conversation turn.
    Message(WireMessageItem),
    /// One call the assistant asked for, replayed from history.
    FunctionCall(WireFunctionCallItem),
    /// One result handed back to the model; the item type where a
    /// result's images ride as content parts.
    FunctionCallOutput(WireFunctionCallOutputItem),
    /// The chain of thought that led to a call, replayed so the model
    /// continues where it left off.
    Reasoning(WireReasoningItem),
}

/// One conversation turn: a role and its content.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessageItem {
    /// `"user"` or `"assistant"`, the roles rhema's history holds.
    pub role: String,
    /// Bare text for a text-only turn, parts for a turn with images.
    pub content: WireItemContent,
}

/// A turn's content: the bare-string shape strict servers expect, or a
/// parts array once images ride along.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum WireItemContent {
    /// The bare text of a text-only turn.
    Text(String),
    /// One part per text block and image, in order.
    Parts(Vec<WireContentPart>),
}

/// One part of a parts-array content: text or an image.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireContentPart {
    /// A text block, verbatim.
    InputText {
        /// The text.
        text: String,
    },
    /// An image as a `data:` URI, the inline form, since rhema carries
    /// bytes, not URLs.
    InputImage {
        /// The whole `data:<media type>;base64,<bytes>` URI.
        image_url: String,
    },
}

/// One function tool offered this step. The Responses-API spelling is
/// flat, the name sits beside the `type`, not under a `function` key.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireResponsesTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The tool's name.
    pub name: String,
    /// The tool's description.
    pub description: String,
    /// The JSON Schema of the call arguments.
    pub parameters: Value,
}

/// The request's thinking level.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireReasoning {
    /// The wire spelling of the level (`low`, `medium`, `high`).
    pub effort: String,
}

/// One call the assistant asked for, replayed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunctionCallItem {
    /// The call's correlation id, the id the paired output carries.
    pub call_id: String,
    /// The tool's name.
    pub name: String,
    /// The call arguments as their JSON text; a `null` payload is `{}`.
    pub arguments: String,
}

/// One result handed back to the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunctionCallOutputItem {
    /// The answered call's correlation id.
    pub call_id: String,
    /// Bare text for a text-only result; a parts array, the text, then
    /// each staged image, when the result carries images.
    pub output: WireItemContent,
}

/// One reasoning item replayed into the context. The chain of thought
/// rides the canonical `summary` field, an array of summary parts,
/// which strict OpenAI-compatible normalizers validate: OpenCode's
/// gateway answers HTTP 400, `input[N].summary: expected an array`,
/// when the field is absent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireReasoningItem {
    /// The chain of thought as summary parts; encrypted forms are not
    /// sent.
    pub summary: Vec<WireReasoningSummary>,
}

/// One summary part of a [`WireReasoningItem`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireReasoningSummary {
    /// Always `"summary_text"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The text itself.
    pub text: String,
}

/// Builds the `POST {base}/responses` body for a request: instructions
/// from the system prompts, one input item per content block, images
/// riding their turn or their tool result as content parts.
pub fn request_body(req: &ModelRequest, dialect: &Dialect) -> ResponsesRequest {
    let mut instructions: Vec<String> = Vec::new();
    if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
        instructions.push(system.to_owned());
    }

    let mut input: Vec<WireInputItem> = Vec::new();
    let mut tools: Vec<WireResponsesTool> = Vec::new();

    for message in &req.messages {
        match message.role {
            Role::System => instructions.push(joined_texts(message)),
            Role::User | Role::Tool => {
                push_message_item(&mut input, message);
                push_result_items(&mut input, message);
            }
            Role::Assistant => {
                push_reasoning_item(&mut input, message);
                push_assistant_items(&mut input, message);
            }
        }
    }

    for spec in &req.tools {
        tools.push(WireResponsesTool {
            kind: "function".to_owned(),
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
        });
    }

    ResponsesRequest {
        model: req.params.model.clone(),
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        input,
        tools,
        stream: true,
        store: false,
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        max_output_tokens: req.params.max_tokens,
        cache_control: (dialect.automatic_cache_control)(&req.params.model),
        session_id: dialect
            .sends_session_id
            .then(|| req.params.session_id.clone())
            .flatten(),
        prompt_cache_key: dialect
            .sends_prompt_cache_key
            .responses()
            .then(|| req.params.session_id.clone())
            .flatten(),
        reasoning: reasoning(req.params.thinking),
    }
}

/// The request's thinking level as the `reasoning` field; `None` sends
/// nothing and the server's default stands.
fn reasoning(thinking: ThinkingEffort) -> Option<WireReasoning> {
    reasoning_effort_value(thinking)
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|effort| WireReasoning { effort })
}

/// Pushes one user turn's message item: bare string when the turn is
/// text-only, a parts array when it carries images. An empty text-only
/// turn pushes nothing.
fn push_message_item(input: &mut Vec<WireInputItem>, message: &Message) {
    if message.role != Role::User {
        return;
    }
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, Content::Image(_)));
    let content = if has_images {
        WireItemContent::Parts(
            message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text(text) => Some(WireContentPart::InputText { text: text.clone() }),
                    Content::Image(image) => Some(WireContentPart::InputImage {
                        image_url: format!(
                            "data:{};base64,{}",
                            image.media_type.as_str(),
                            image.data
                        ),
                    }),
                    _ => None,
                })
                .collect(),
        )
    } else {
        let text = joined_texts(message);
        if text.is_empty() {
            return;
        }
        WireItemContent::Text(text)
    };
    input.push(WireInputItem::Message(WireMessageItem {
        role: "user".to_owned(),
        content,
    }));
}

/// Pushes one `function_call_output` item per result block: bare string
/// output for a text-only result, a parts array, the result text
/// first, then each staged image as an `input_image` data URI, when
/// the result carries images.
fn push_result_items(input: &mut Vec<WireInputItem>, message: &Message) {
    for content in &message.content {
        let Content::ToolResult(result) = content else {
            continue;
        };
        let output = if result.images.is_empty() {
            WireItemContent::Text(result.content.clone())
        } else {
            let mut parts = vec![WireContentPart::InputText {
                text: result.content.clone(),
            }];
            parts.extend(
                result
                    .images
                    .iter()
                    .map(|image| WireContentPart::InputImage {
                        image_url: format!(
                            "data:{};base64,{}",
                            image.media_type.as_str(),
                            image.data
                        ),
                    }),
            );
            WireItemContent::Parts(parts)
        };
        input.push(WireInputItem::FunctionCallOutput(
            WireFunctionCallOutputItem {
                call_id: result.id.clone(),
                output,
            },
        ));
    }
}

/// Pushes the replayed reasoning of a message's calls: the chain of
/// thought each call carried out of the stream it streamed in, as one
/// reasoning item ahead of the calls, in stream order.
fn push_reasoning_item(input: &mut Vec<WireInputItem>, message: &Message) {
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
        .collect::<Vec<_>>()
        .join("\n\n");
    if reasoning.is_empty() {
        return;
    }
    input.push(WireInputItem::Reasoning(WireReasoningItem {
        summary: vec![WireReasoningSummary {
            kind: "summary_text".to_owned(),
            text: reasoning,
        }],
    }));
}

/// Pushes an assistant turn's items: the text message, then one
/// `function_call` item per call. An empty turn pushes nothing.
fn push_assistant_items(input: &mut Vec<WireInputItem>, message: &Message) {
    let text = joined_texts(message);
    if !text.is_empty() {
        input.push(WireInputItem::Message(WireMessageItem {
            role: "assistant".to_owned(),
            content: WireItemContent::Text(text),
        }));
    }
    for content in &message.content {
        let Content::ToolCall(call) = content else {
            continue;
        };
        input.push(WireInputItem::FunctionCall(WireFunctionCallItem {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: arguments_text(&call.args),
        }));
    }
}

// --- Stream event wire types (deserialize) ---

/// One typed server-sent event of a Responses stream. Every event
/// carries its `type` in the payload; the fields each type fills are
/// optional, so unknown event types decode and are ignored downstream.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireStreamEvent {
    /// The event's type, e.g. `response.output_text.delta`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The output item an item-level event carries.
    pub item: Option<WireOutputItem>,
    /// A piece of text or of a call's arguments.
    pub delta: Option<String>,
    /// The call id an arguments event names, when the server sends it.
    pub call_id: Option<String>,
    /// The item id an arguments event names, when it names that instead.
    pub item_id: Option<String>,
    /// The full response object the terminal events carry.
    pub response: Option<WireResponseBody>,
    /// A mid-stream error's message (`{"type":"error"}`).
    pub message: Option<String>,
    /// A mid-stream error's code (`{"type":"error"}`).
    pub code: Option<String>,
}

/// One output item of the response: a message, a reasoning block, a
/// function call, or a server-side tool the assembler ignores. Only the
/// fields a function call needs are kept.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireOutputItem {
    /// The item's type (`message`, `reasoning`, `function_call`, ...).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// The item's id, when the server sends one.
    pub id: Option<String>,
    /// The item id spelled `item_id` (some servers).
    pub item_id: Option<String>,
    /// The function call's correlation id.
    pub call_id: Option<String>,
    /// The called tool's name.
    pub name: Option<String>,
    /// The call's arguments as their JSON text, complete on a `done`
    /// item.
    pub arguments: Option<String>,
}

/// The full response object a terminal event carries.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireResponseBody {
    /// `completed`, `incomplete`, or `failed`.
    pub status: Option<String>,
    /// Why an incomplete response stopped.
    pub incomplete_details: Option<WireIncompleteDetails>,
    /// Token accounting, when the server reports it.
    pub usage: Option<WireResponsesUsage>,
    /// The failure's details, on a failed response.
    pub error: Option<WireResponseError>,
}

/// Why an incomplete response stopped.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireIncompleteDetails {
    /// The reason, e.g. `max_output_tokens`.
    pub reason: Option<String>,
}

/// Token accounting of a Responses call.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireResponsesUsage {
    /// Prompt tokens billed.
    pub input_tokens: Option<u32>,
    /// Completion tokens billed.
    pub output_tokens: Option<u32>,
    /// The prompt's cache details.
    pub input_tokens_details: Option<WireInputTokensDetails>,
}

/// The prompt's cache details.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireInputTokensDetails {
    /// Prompt tokens served from the server's cache.
    pub cached_tokens: Option<u32>,
}

/// A failed response's details.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WireResponseError {
    /// The failure's code, when named.
    pub code: Option<String>,
    /// The failure's message.
    pub message: Option<String>,
}

// --- Mapping: stream events -> ModelOutput / events ---

/// One streamed function call being assembled across events.
#[derive(Debug, Default)]
struct PartialCall {
    /// The wire call id, when the server sent one.
    call_id: Option<String>,
    /// The wire item id, when the server names one.
    item_id: Option<String>,
    /// The correlation id rhema uses: the wire call id, else the item
    /// id, else a synthesized `call_{n}`.
    id: String,
    name: String,
    arguments: String,
    started: bool,
    /// Whether the call's `done` item has been folded: a server that
    /// spells no ids cannot have its items matched back by id, and the
    /// pairing has to stay positional instead.
    done: bool,
    /// The reasoning streamed since the previous call: claimed by this
    /// call so the replay puts it back ahead of it.
    reasoning: String,
}

/// Why a response stopped, from the terminal event.
#[derive(Debug, Clone, PartialEq)]
enum Finish {
    /// `response.completed`.
    Completed,
    /// `response.incomplete`, with its reason when named.
    Incomplete(Option<String>),
}

/// Assembles streamed Responses events into the final [`ModelOutput`],
/// emitting streaming events as deltas arrive. The same contract as the
/// chat [`crate::wire::ChunkAssembler`]: the assistant text is one part
/// (delta boundaries are transport artifacts), calls keep start order,
/// and a stream with neither yields an empty content list, not an
/// error.
#[derive(Debug, Default)]
pub struct ResponsesAssembler {
    /// The assistant text streamed so far.
    text: String,
    /// The reasoning streamed since the last call began; claimed by the
    /// next call.
    reasoning: String,
    /// The calls, in start order.
    calls: Vec<PartialCall>,
    /// The terminal event's verdict, when one arrived.
    finish: Option<Finish>,
    usage: Usage,
}

impl ResponsesAssembler {
    /// An assembler that has seen no events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one event, emitting its streaming events. Fails with
    /// [`ProviderError::Api`] when the event is the server's
    /// `response.failed` or a mid-stream `error` event; unknown event
    /// types are ignored, so a server may grow its vocabulary without
    /// breaking the assembler.
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Api`] (no status) on a
    /// `response.failed` or `error` event.
    pub fn push(
        &mut self,
        event: WireStreamEvent,
        sink: &mut dyn EventSink,
    ) -> Result<(), ProviderError> {
        match event.kind.as_str() {
            "response.output_item.added" => {
                if let Some(item) = event.item {
                    self.item_added(&item, sink);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    self.item_done(&item, sink);
                }
            }
            "response.output_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    sink.emit(Event::TextDelta(&text));
                    self.text.push_str(&text);
                }
            }
            // DeepSeek spells its raw chain-of-thought deltas
            // `reasoning_text`; OpenAI streams summaries the same shape.
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    sink.emit(Event::ThinkingDelta(&text));
                    self.reasoning.push_str(&text);
                }
            }
            "response.function_call_arguments.delta" => {
                // The wire allows a delta to arrive before its
                // `output_item.added` establishes the call; an
                // unnamed delta attaches to the most recent call.
                // Nothing established yet means nothing to attach
                // to: drop the fragment, the way the chat-side
                // assembler drops stray fragments.
                if let Some(piece) = event
                    .delta
                    .filter(|piece| !piece.is_empty())
                    .filter(|_| !self.calls.is_empty())
                {
                    let index =
                        self.target_index(event.call_id.as_deref(), event.item_id.as_deref());
                    self.claim_reasoning_for(index);
                    let partial = &mut self.calls[index];
                    sink.emit(Event::ToolCallArgsDelta {
                        id: &partial.id,
                        delta: &piece,
                    });
                    partial.arguments.push_str(&piece);
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(response) = &event.response {
                    if let Some(usage) = &response.usage {
                        self.usage = Usage {
                            input_tokens: usage.input_tokens.unwrap_or(0),
                            output_tokens: usage.output_tokens.unwrap_or(0),
                            cached_input_tokens: usage
                                .input_tokens_details
                                .as_ref()
                                .and_then(|details| details.cached_tokens)
                                .unwrap_or(0),
                        };
                        // The terminal event is the only usage carrier on
                        // this wire, so this fires once, right before
                        // `StepEnd` says the same.
                        sink.emit(Event::Usage(self.usage));
                    }
                    self.finish = Some(if event.kind == "response.completed" {
                        Finish::Completed
                    } else {
                        Finish::Incomplete(
                            response
                                .incomplete_details
                                .as_ref()
                                .and_then(|details| details.reason.clone()),
                        )
                    });
                }
            }
            "response.failed" => {
                let message = event
                    .response
                    .as_ref()
                    .and_then(|response| response.error.as_ref())
                    .and_then(|error| error.message.clone())
                    .unwrap_or_else(|| "response failed".to_owned());
                return Err(ProviderError::Api {
                    message,
                    status: None,
                });
            }
            // A mid-stream error event: OpenRouter and OpenAI both
            // spell one, with the message at the event's top level.
            // Failing here keeps a silent empty success impossible.
            "error" => {
                let message = event
                    .message
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "mid-stream error".to_owned());
                return Err(ProviderError::Api {
                    message,
                    status: None,
                });
            }
            // `response.created`, `response.in_progress`, the
            // content-part and done bookkeeping, and every server-side
            // tool event: nothing the assembler keeps.
            _ => {}
        }
        Ok(())
    }

    /// Folds a call's `added` event: the announcement of the call, with
    /// argument pieces following as deltas. A call whose name has not
    /// arrived yet is remembered but not started, the `done` item
    /// starts it.
    fn item_added(&mut self, item: &WireOutputItem, sink: &mut dyn EventSink) {
        if item.kind.as_deref() != Some("function_call") {
            return;
        }
        let call_id = item.call_id.clone().filter(|id| !id.is_empty());
        let item_id = item
            .item_id
            .clone()
            .or_else(|| item.id.clone())
            .filter(|id| !id.is_empty());
        let id = call_id
            .clone()
            .or_else(|| item_id.clone())
            .unwrap_or_else(|| format!("call_{}", self.calls.len()));
        let name = item.name.clone().unwrap_or_default();
        let started = !name.is_empty();
        self.calls.push(PartialCall {
            call_id,
            item_id,
            id,
            name,
            started,
            ..PartialCall::default()
        });
        if started {
            let index = self.calls.len() - 1;
            self.claim_reasoning_for(index);
            let partial = &self.calls[index];
            sink.emit(Event::ToolCallStart {
                id: &partial.id,
                name: &partial.name,
            });
        }
    }

    /// Folds a call's `done` event: the complete item. A call still
    /// unstarted starts here, some servers spell the whole call only
    /// at `done`, and the item's complete arguments replace the
    /// accumulated pieces when present.
    fn item_done(&mut self, item: &WireOutputItem, sink: &mut dyn EventSink) {
        if item.kind.as_deref() != Some("function_call") {
            return;
        }
        let call_id = item.call_id.as_deref().filter(|id| !id.is_empty());
        let item_id = item
            .item_id
            .as_deref()
            .or(item.id.as_deref())
            .filter(|id| !id.is_empty());
        let index = self
            .find_index(call_id, item_id)
            // An id-less server's items cannot match by id: its
            // added/done pairs arrive in order, so the done falls to
            // the oldest call that has not taken one yet. Without
            // this, a done for an already-added call registers the
            // call a second time and the message carries it twice.
            .or_else(|| self.calls.iter().position(|call| !call.done))
            .unwrap_or(self.calls.len());
        if index == self.calls.len() {
            // `item_added` always registers the item at this point: the
            // kind gate above leaves only function calls, and its push
            // takes every one.
            self.item_added(item, sink);
        }
        let partial = &mut self.calls[index];
        partial.done = true;
        if !partial.started {
            partial.started = true;
            partial.name = item.name.clone().unwrap_or_default();
            let pending = std::mem::take(&mut self.reasoning);
            if !pending.is_empty() {
                partial.reasoning.push_str(&pending);
            }
            sink.emit(Event::ToolCallStart {
                id: &partial.id,
                name: &partial.name,
            });
        }
        if let Some(arguments) = &item.arguments {
            partial.arguments = arguments.clone();
        }
    }

    /// The index of the call an arguments delta names: by call id, else
    /// by item id, else the last call seen.
    fn target_index(&self, call_id: Option<&str>, item_id: Option<&str>) -> usize {
        self.find_index(call_id, item_id)
            .unwrap_or(self.calls.len().saturating_sub(1))
    }

    /// The index of the call matching either id; `None` when no call
    /// carries them.
    fn find_index(&self, call_id: Option<&str>, item_id: Option<&str>) -> Option<usize> {
        self.calls.iter().position(|call| {
            same_nonempty(call.call_id.as_deref(), call_id)
                || same_nonempty(call.item_id.as_deref(), item_id)
        })
    }

    /// Moves the pending reasoning onto the call at `index`: the chain
    /// of thought streamed since the previous call led to this one.
    fn claim_reasoning_for(&mut self, index: usize) {
        let pending = std::mem::take(&mut self.reasoning);
        if !pending.is_empty() {
            self.calls[index].reasoning.push_str(&pending);
        }
    }

    /// Consumes the assembler into the final output. The message is
    /// always `Role::Assistant`: the streamed text as one part, then
    /// tool calls in start order, each carrying the reasoning that led
    /// to it in its `provider_data`.
    /// Whether a terminal event (`response.completed` /
    /// `response.incomplete`) established why the stream stopped. A
    /// `response.failed` fails the push outright, so this is the
    /// completed-or-incomplete proof the drain checks before standing
    /// the assembled output up as a result.
    pub fn was_completed(&self) -> bool {
        self.finish.is_some()
    }

    /// The assembled output: the streamed text, the named calls in
    /// their start order (a nameless fragment is dropped, no half-call
    /// rides the transcript), and the usage the terminal event carried.
    pub fn finish(self) -> ModelOutput {
        let Self {
            text,
            calls,
            finish,
            usage,
            ..
        } = self;
        let mut content: Vec<Content> = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        let mut has_calls = false;
        for partial in calls {
            // A name that never arrived leaves no callable tool: the
            // transcript must not carry a half-call, and it must not
            // force a ToolUse verdict the message cannot back, the
            // sibling adapters' rule.
            if partial.name.is_empty() {
                continue;
            }
            has_calls = true;
            let provider_data = if partial.reasoning.is_empty() {
                Value::Null
            } else {
                serde_json::json!({ "reasoning": partial.reasoning })
            };
            content.push(Content::ToolCall(ToolCall {
                id: partial.id,
                name: partial.name,
                args: parse_args(&partial.arguments, OPENAI_COMPATIBLE.args_key),
                provider_data,
            }));
        }
        ModelOutput {
            message: Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content,
            },
            stop_reason: stop_reason(finish.as_ref(), has_calls),
            usage,
        }
    }
}

/// Maps the terminal event onto rhema-core's stop reason; any named
/// tool call in the assembled message wins as [`StopReason::ToolUse`],
/// the same rule as the sibling adapters.
fn stop_reason(finish: Option<&Finish>, has_calls: bool) -> StopReason {
    if has_calls {
        return StopReason::ToolUse;
    }
    match finish {
        None | Some(Finish::Completed) => StopReason::EndTurn,
        Some(Finish::Incomplete(Some(reason))) if reason == "max_output_tokens" => {
            StopReason::MaxTokens
        }
        Some(Finish::Incomplete(reason)) => {
            StopReason::Other(reason.clone().unwrap_or("incomplete".to_owned()))
        }
    }
}

/// Whether two optional ids name the same thing, both present, both
/// non-empty, equal.
fn same_nonempty(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => !left.is_empty() && left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::event::Event;
    use rhema_core::message::{Image, ImageMediaType};
    use rhema_core::request::ChatParams;

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

    fn event(json: &str) -> WireStreamEvent {
        serde_json::from_str(json).expect("event fixture parses")
    }

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            system: None,
            messages,
            tools: vec![],
            params: ChatParams::new("deepseek-flash"),
        }
    }

    #[test]
    fn a_request_maps_system_tools_and_text_turns() {
        let req = ModelRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message::text(Role::User, "list files"),
                Message::text(Role::Assistant, "checking"),
            ],
            tools: vec![rhema_core::request::ToolSpec {
                name: "bash".into(),
                description: "run a command".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            params: ChatParams {
                top_p: Some(0.8),
                ..ChatParams::new("deepseek-flash")
            },
        };

        let body =
            serde_json::to_value(request_body(&req, &OPENAI_COMPATIBLE)).expect("body serializes");
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["top_p"], serde_json::json!(0.8_f32));
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        // The tools are flat: the name beside the type, not under a
        // `function` key.
        assert_eq!(
            body["tools"][0],
            serde_json::json!({
                "type": "function",
                "name": "bash",
                "description": "run a command",
                "parameters": {"type": "object"}
            })
        );
        // Text-only turns keep the bare-string content shape.
        assert_eq!(
            body["input"][0],
            serde_json::json!({
                "type": "message", "role": "user", "content": "list files"
            })
        );
        assert_eq!(
            body["input"][1],
            serde_json::json!({
                "type": "message", "role": "assistant", "content": "checking"
            })
        );
        assert_eq!(body["input"].as_array().expect("input").len(), 2);
    }

    #[test]
    fn the_cache_routing_key_rides_where_the_dialect_takes_it() {
        use crate::wire::{OPENAI, PromptCacheKeyRoute};
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "go")],
            tools: vec![],
            params: ChatParams::new("gpt-5"),
        };
        req.params.session_id = Some("20261120120000-00".into());
        // OpenAI's home dialect: the Responses body carries the key.
        let body = serde_json::to_value(request_body(&req, &OPENAI)).expect("serializes");
        assert_eq!(body["prompt_cache_key"], "20261120120000-00");
        // The loosest dialect: absent, an unknown server sees nothing.
        let body =
            serde_json::to_value(request_body(&req, &OPENAI_COMPATIBLE)).expect("serializes");
        assert!(body.get("prompt_cache_key").is_none());
        // Chat-only dialects (Mistral): the Responses body stays clean.
        let chat_only = Dialect {
            sends_prompt_cache_key: PromptCacheKeyRoute::Chat,
            ..OPENAI_COMPATIBLE
        };
        let body = serde_json::to_value(request_body(&req, &chat_only)).expect("serializes");
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn system_messages_fold_into_instructions_after_normalization() {
        let req = ModelRequest {
            system: Some("a".into()),
            messages: vec![
                Message::text(Role::System, "b"),
                Message::text(Role::User, "hi"),
            ],
            tools: vec![],
            params: ChatParams::new("deepseek-flash"),
        }
        .normalized();
        let body = request_body(&req, &OPENAI_COMPATIBLE);
        assert_eq!(body.instructions.as_deref(), Some("a\n\nb"));
        assert_eq!(body.input.len(), 1);
    }

    #[test]
    fn a_users_images_ride_its_turn_and_a_results_images_ride_its_output() {
        let req = request(vec![
            Message::text_with_images(
                Role::User,
                "what is this",
                vec![Image::new(ImageMediaType::Png, "cG5n")],
            ),
            Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![
                    Content::ToolResult(rhema_core::ToolResult::success("call_0", "screenshot")),
                    Content::ToolResult(
                        rhema_core::ToolResult::success("call_1", "chart")
                            .with_images(vec![Image::new(ImageMediaType::Jpeg, "anM=")]),
                    ),
                ],
            },
        ]);

        let body =
            serde_json::to_value(request_body(&req, &OPENAI_COMPATIBLE)).expect("body serializes");
        let input = body["input"].as_array().expect("input");
        // The image rides its user turn as an input_image data-URI part.
        assert_eq!(
            input[0],
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what is this"},
                    {"type": "input_image", "image_url": "data:image/png;base64,cG5n"}
                ]
            })
        );
        // A text-only result keeps the bare-string output.
        assert_eq!(
            input[1],
            serde_json::json!({
                "type": "function_call_output", "call_id": "call_0", "output": "screenshot"
            })
        );
        // A result with images spells a parts array: text first, then
        // the image, inside the function_call_output itself, no
        // follow-up turn.
        assert_eq!(
            input[2],
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [
                    {"type": "input_text", "text": "chart"},
                    {"type": "input_image", "image_url": "data:image/jpeg;base64,anM="}
                ]
            })
        );
    }

    #[test]
    fn assistant_calls_replay_with_their_reasoning_ahead_of_them() {
        let req = request(vec![Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![
                Content::Text("running it".into()),
                Content::ToolCall(ToolCall {
                    id: "call_7".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                    provider_data: serde_json::json!({"reasoning": "need the listing"}),
                }),
            ],
        }]);

        let body =
            serde_json::to_value(request_body(&req, &OPENAI_COMPATIBLE)).expect("body serializes");
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 3, "{input:?}");
        assert_eq!(
            input[0],
            serde_json::json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "need the listing"}]
            })
        );
        assert_eq!(
            input[1],
            serde_json::json!({
                "type": "message", "role": "assistant", "content": "running it"
            })
        );
        assert_eq!(
            input[2],
            serde_json::json!({
                "type": "function_call",
                "call_id": "call_7",
                "name": "bash",
                "arguments": "{\"cmd\":\"ls\"}"
            })
        );
    }

    #[test]
    fn a_null_args_payload_becomes_an_empty_object_and_effort_maps_to_reasoning() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_0".into(),
                    name: "see".into(),
                    args: Value::Null,
                    provider_data: Value::Null,
                })],
            }],
            tools: vec![],
            params: ChatParams {
                thinking: ThinkingEffort::High,
                ..ChatParams::new("deepseek-flash")
            },
        };

        let body =
            serde_json::to_value(request_body(&req, &OPENAI_COMPATIBLE)).expect("body serializes");
        assert_eq!(body["input"][0]["arguments"], "{}");
        assert_eq!(body["reasoning"], serde_json::json!({"effort": "high"}));
    }

    #[test]
    fn text_and_reasoning_deltas_stream_and_the_call_claims_the_reasoning() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.reasoning_text.delta","delta":"thinking "}"#,
            r#"{"type":"response.reasoning_text.delta","delta":"hard"}"#,
            r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
            r#"{"type":"response.output_text.delta","delta":"lo"}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","call_id":"fc_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_1","delta":"{\"c"}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_1","delta":"md\":1}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","call_id":"fc_1","name":"bash",
                "arguments":"{\"cmd\":\"ls\"}","status":"completed"}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();

        // The reasoning deltas stream as thinking deltas and end up on
        // the call they led to; the text deltas as one text part.
        assert_eq!(
            sink.0,
            vec![
                "ThinkingDelta(\"thinking \")",
                "ThinkingDelta(\"hard\")",
                "TextDelta(\"Hel\")",
                "TextDelta(\"lo\")",
                "ToolCallStart { id: \"fc_1\", name: \"bash\" }",
                "ToolCallArgsDelta { id: \"fc_1\", delta: \"{\\\"c\" }",
                "ToolCallArgsDelta { id: \"fc_1\", delta: \"md\\\":1}\" }",
            ]
        );
        assert_eq!(out.message.role, Role::Assistant);
        assert_eq!(
            out.message.content,
            vec![
                Content::Text("Hello".into()),
                Content::ToolCall(ToolCall {
                    id: "fc_1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                    provider_data: serde_json::json!({"reasoning": "thinking hard"}),
                }),
            ]
        );
        assert_eq!(out.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn a_delta_ahead_of_its_call_is_dropped_not_a_panic() {
        // A server (or proxy) may order `function_call_arguments.delta`
        // before the `output_item.added` that names the call: the
        // fragment has nothing to attach to, so it is dropped and the
        // stream keeps going, rather than indexing an empty vec.
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_1","delta":"{\"c"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_0","delta":"md"}"#,
            r#"{"type":"response.output_text.delta","delta":"late"}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","call_id":"fc_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_1","delta":"{\"cmd\":\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","call_id":"fc_1","name":"bash",
                "arguments":"{\"cmd\":\"ls\"}","status":"completed"}}"#,
            r#"{"type":"response.completed","response":{"usage":{
                "input_tokens":1,"output_tokens":1}}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();

        // The early fragments are gone; the call and the text the
        // established stream carries survive intact.
        assert_eq!(
            sink.0,
            vec![
                "TextDelta(\"late\")",
                "ToolCallStart { id: \"fc_1\", name: \"bash\" }",
                "ToolCallArgsDelta { id: \"fc_1\", delta: \"{\\\"cmd\\\":\\\"ls\\\"}\" }",
                "Usage(Usage { input_tokens: 1, output_tokens: 1, cached_input_tokens: 0 })",
            ]
        );
        assert_eq!(out.message.content.len(), 2);
        assert_eq!(out.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn an_idless_calls_done_pairs_with_its_added_not_a_second_registration() {
        // A server may spell no ids at all: the done item then cannot
        // match its call by id, and it must fall to the call the added
        // item registered, positionally, instead of registering the
        // call a second time and announcing it twice.
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","name":"bash"}}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"cmd\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","name":"bash",
                "arguments":"{\"cmd\":\"ls\"}","status":"completed"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();

        // One announcement, one call, the pieces and the done item's
        // whole text agreeing on the arguments.
        assert_eq!(
            sink.0,
            vec![
                "ToolCallStart { id: \"call_0\", name: \"bash\" }",
                "ToolCallArgsDelta { id: \"call_0\", delta: \"{\\\"cmd\\\":\" }",
                "ToolCallArgsDelta { id: \"call_0\", delta: \"\\\"ls\\\"}\" }",
            ]
        );
        assert_eq!(out.message.content.len(), 1, "one call, not two");
        assert_eq!(
            out.message.content[0],
            Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "bash".into(),
                args: serde_json::json!({"cmd": "ls"}),
                provider_data: Value::Null,
            })
        );
    }

    #[test]
    fn a_done_only_call_starts_at_done_and_completed_carries_usage() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","call_id":"fc_2","name":"see",
                "arguments":"{}"}}"#,
            r#"{"type":"response.completed","response":{
                "status":"completed",
                "usage":{"input_tokens":10,"output_tokens":2,
                         "input_tokens_details":{"cached_tokens":4}}}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();

        // The done-only call announced itself, and usage streamed once.
        assert_eq!(
            sink.0,
            vec![
                "ToolCallStart { id: \"fc_2\", name: \"see\" }",
                "Usage(Usage { input_tokens: 10, output_tokens: 2, cached_input_tokens: 4 })",
            ]
        );
        assert_eq!(out.usage.input_tokens, 10);
        assert_eq!(out.usage.cached_input_tokens, 4);
        assert_eq!(out.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn an_incomplete_response_maps_to_max_tokens_and_unknown_events_pass() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.in_progress","response":{}}"#,
            r#"{"type":"response.content_part.added","part":{"type":"output_text"}}"#,
            r#"{"type":"response.web_search_call.in_progress"}"#,
            r#"{"type":"response.output_text.delta","delta":"part"}"#,
            r#"{"type":"response.incomplete","response":{
                "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"},
                "usage":{"input_tokens":5,"output_tokens":9}}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();
        assert_eq!(out.message.content, vec![Content::Text("part".into())]);
        assert_eq!(out.stop_reason, StopReason::MaxTokens);
        assert_eq!(out.usage.output_tokens, 9);
    }

    #[test]
    fn a_failed_response_fails_the_push() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(
                event(
                    r#"{"type":"response.failed","response":{
                        "status":"failed",
                        "error":{"code":"server_error","message":"boom"}}}"#,
                ),
                &mut sink,
            )
            .expect_err("failed response is an error");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn an_error_event_fails_the_push_with_its_message() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(
                event(r#"{"type":"error","code":"upstream_error","message":"provider exploded"}"#),
                &mut sink,
            )
            .expect_err("an error event is an error");
        assert!(err.to_string().contains("provider exploded"), "{err}");

        // A nameless error still fails, never masquerades as success.
        let err = assembler
            .push(
                event(r#"{"type":"error","code":"upstream_error"}"#),
                &mut sink,
            )
            .expect_err("an error event is an error");
        assert!(err.to_string().contains("mid-stream error"), "{err}");
    }

    #[test]
    fn an_empty_stream_yields_an_empty_assistant_message() {
        let out = ResponsesAssembler::new().finish();
        assert_eq!(out.message.role, Role::Assistant);
        assert_eq!(out.message.content, Vec::new());
        assert_eq!(out.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn a_terminal_event_is_the_completed_proof() {
        // Nothing terminal, nothing completed: an empty stream never
        // stood up as a result, the drain behind it refuses.
        assert!(!ResponsesAssembler::new().was_completed());
    }

    #[test]
    fn a_completed_event_is_the_completed_proof() {
        // A terminal event seen: the drain stands the output up.
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                event(
                    r#"{"type":"response.completed","response":{"usage":{
                "input_tokens":1,"output_tokens":1}}}"#,
                ),
                &mut sink,
            )
            .expect("event is not an error");
        assert!(assembler.was_completed());
    }

    #[test]
    fn a_nameless_response_call_is_dropped_for_text() {
        // A malformed stream can announce a call whose name element never
        // arrives: the text survives, the half-call never rides the
        // transcript, no half-call forces a tool-use verdict the
        // message cannot back, the sibling rule.
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.output_text.delta","delta":"half"}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","call_id":"fc_z","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_z","delta":"{}"}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();
        assert_eq!(out.message.content, vec![Content::Text("half".into())]);
        assert_eq!(out.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn parallel_calls_keep_their_start_order_and_ids() {
        let mut assembler = ResponsesAssembler::new();
        let mut sink = Collector::new();
        for fixture in [
            r#"{"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","call_id":"fc_a","name":"bash","arguments":""}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","call_id":"fc_b","name":"see","arguments":""}}"#,
            // Interleaved pieces: the ids name the call, not the order.
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_b","delta":"{}"}"#,
            r#"{"type":"response.function_call_arguments.delta","call_id":"fc_a","delta":"{\"x\":1}"}"#,
        ] {
            assembler
                .push(event(fixture), &mut sink)
                .expect("event is not an error");
        }
        let out = assembler.finish();
        let calls: Vec<&ToolCall> = out
            .message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "fc_a");
        assert_eq!(calls[0].args, serde_json::json!({"x": 1}));
        assert_eq!(calls[1].id, "fc_b");
        assert_eq!(calls[1].args, serde_json::json!({}));
    }
}
