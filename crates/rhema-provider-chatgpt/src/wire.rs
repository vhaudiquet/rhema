//! The ChatGPT backend's wire types and the pure mapping between them
//! and rhema-core's model. No I/O here; [`crate::provider`] drives
//! these functions.
//!
//! The endpoint is the Responses API under the ChatGPT backend
//! (`https://chatgpt.com/backend-api/codex`). Mapping constraints:
//!
//! - The system prompt is the request's `instructions` string:
//!   `ModelRequest::system` and every `Role::System` message, joined by
//!   a blank line. The Responses input array has no system role this
//!   adapter needs to spell separately.
//! - `Role::User` becomes one `message` item with `input_text` parts
//!   for its text blocks and `input_image` parts for its images, each
//!   image url a `data:{media type};base64,…` URI. `Role::Tool` result
//!   blocks become `function_call_output` items, matched by call id,
//!   whichever message carries them; images ride an output content
//!   array (`input_text` + `input_image`) since the plain-string form
//!   cannot carry them.
//! - `Role::Assistant` text becomes one `message` item of `output_text`
//!   parts (the shape the Responses API replays assistant turns in);
//!   each tool call becomes a `function_call` item carrying its
//!   `call_id`, name, and JSON-encoded arguments. An assistant turn
//!   with neither is dropped, since the API rejects an empty message.
//! - `ToolCall::provider_data` has no ChatGPT home: call ids are plain
//!   strings and the backend mints no opaque per-call marker this
//!   adapter must replay. It is ignored on the way in.
//! - Tool specs map to Responses function tools:
//!   `{type: "function", name, description, strict: false, parameters}`,
//!   with `parameters` passed through verbatim.
//! - Every call streams (`stream: true`, `store: false`) and asks for
//!   no reasoning replay: rhema's history keeps text and tool calls,
//!   not the encrypted reasoning the backend can return. The wire is
//!   stateless, so nothing else is needed.
//! - `ThinkingEffort` rides `reasoning.effort` (`low`/`medium`/`high`);
//!   the default sends no reasoning object at all, leaving the model's
//!   own default. A named level also asks for `summary: "auto"`, whose
//!   deltas become `ThinkingDelta`.
//! - Sampling temperature is not sent: this endpoint serves reasoning
//!   models, which reject it. The output ceiling rides
//!   `max_output_tokens` when the request sets one.
//! - SSE events: `response.output_text.delta` becomes `TextDelta`;
//!   `response.reasoning_summary_text.delta` and
//!   `response.reasoning_text.delta` become `ThinkingDelta`;
//!   `response.output_item.added`/`.done` for a `function_call` emit
//!   `ToolCallStart` once; `response.function_call_arguments.delta`
//!   emits `ToolCallArgsDelta` keyed by call id. The terminal
//!   `response.completed`/`response.incomplete` event is authoritative:
//!   its `output` array replaces the streamed text and calls, its
//!   `usage` becomes the final [`Usage`], and any tool call wins the
//!   stop reason as [`StopReason::ToolUse`]. An incomplete response
//!   with `incomplete_details.reason == "max_output_tokens"` maps to
//!   [`StopReason::MaxTokens`] when it carries no calls; a
//!   `content_filter` reason is an API error.
//! - A `response.failed` event, a top-level `error` event, or an
//!   `error` object on the terminal response surfaces as
//!   [`ProviderError::Api`] without a status (the HTTP call succeeded).
//! - Arguments are parsed into JSON per call; an empty string becomes
//!   `{}` and an unparseable payload wraps as
//!   `{"_chatgpt_args_text": <raw>}`, so a malformed call survives into
//!   the transcript instead of ending the turn.
//! - Models: `GET {api}/models` returns `{"models": [{slug,
//!   display_name, description, context_window, max_context_window,
//!   input_modalities, visibility, supported_in_api, …}]}`. Hidden
//!   rows and rows the API cannot serve are filtered; the rest become
//!   [`ModelCard`]s with the chat marker every adapter uses. No price
//!   rides a subscription plan.

use std::collections::{BTreeMap, HashMap};

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::{ModelRequest, ThinkingEffort};
use rhema_core::tool::ToolCall;
use rhema_provider::ModelCard;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Builds the Responses API body for one turn.
pub fn request_body(req: &ModelRequest) -> ResponsesRequest {
    ResponsesRequest {
        model: req.params.model.clone(),
        instructions: instructions(req),
        input: input_items(req),
        tools: tools(req),
        tool_choice: "auto",
        parallel_tool_calls: true,
        reasoning: reasoning(req.params.thinking),
        store: false,
        stream: true,
        max_output_tokens: req.params.max_tokens,
        prompt_cache_key: None,
    }
}

/// The request the ChatGPT backend answers.
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    /// Model slug.
    pub model: String,
    /// The system prompt, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Conversation items in order.
    pub input: Vec<Value>,
    /// Function tools, when the turn offers any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    /// Always `auto`: rhema leaves the choosing to the model.
    pub tool_choice: &'static str,
    /// Parallel calls are safe: results are matched by call id.
    pub parallel_tool_calls: bool,
    /// Reasoning controls, absent at the default effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// The backend is stateless here; nothing is stored server-side.
    pub store: bool,
    /// SSE, always.
    pub stream: bool,
    /// The output ceiling, when the caller set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// A stable key for the conversation's prompt cache, when the
    /// caller has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

/// The Responses `reasoning` object.
#[derive(Debug, Clone, Serialize)]
pub struct Reasoning {
    /// `low`, `medium`, or `high`.
    pub effort: &'static str,
    /// Ask for a streamed summary of the reasoning.
    pub summary: &'static str,
}

/// The system prompt, folded from the request and every system message.
fn instructions(req: &ModelRequest) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = req.system.as_deref().filter(|text| !text.trim().is_empty()) {
        parts.push(system.to_owned());
    }
    for message in &req.messages {
        if message.role != Role::System {
            continue;
        }
        let text = texts(message);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    let joined = parts.join("\n\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// The conversation's input items, in order.
fn input_items(req: &ModelRequest) -> Vec<Value> {
    let mut items = Vec::new();
    for message in &req.messages {
        match message.role {
            // Folded into `instructions`.
            Role::System => {}
            Role::User => {
                let parts = user_parts(message);
                if !parts.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": parts,
                    }));
                }
                items.extend(tool_outputs(message));
            }
            Role::Assistant => {
                let text = texts(message);
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for content in &message.content {
                    if let Content::ToolCall(call) = content {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": arguments_text(&call.args),
                        }));
                    }
                }
                items.extend(tool_outputs(message));
            }
            Role::Tool => items.extend(tool_outputs(message)),
        }
    }
    items
}

/// A user turn's content parts: text blocks and images in order.
fn user_parts(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(json!({"type": "input_text", "text": text})),
            Content::Image(image) => Some(json!({
                "type": "input_image",
                "image_url": image_url(image),
            })),
            Content::ToolCall(_) | Content::ToolResult(_) => None,
        })
        .collect()
}

/// One `function_call_output` item per result block of `message`.
fn tool_outputs(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::ToolResult(result) => {
                let output = if result.images.is_empty() {
                    Value::String(result.content.clone())
                } else {
                    let mut parts = Vec::new();
                    if !result.content.is_empty() {
                        parts.push(json!({"type": "input_text", "text": result.content}));
                    }
                    parts.extend(result.images.iter().map(
                        |image| json!({"type": "input_image", "image_url": image_url(image)}),
                    ));
                    Value::Array(parts)
                };
                Some(json!({
                    "type": "function_call_output",
                    "call_id": result.id,
                    "output": output,
                }))
            }
            _ => None,
        })
        .collect()
}

/// The function tools, when the turn offers any.
fn tools(req: &ModelRequest) -> Option<Vec<Value>> {
    (!req.tools.is_empty()).then(|| {
        req.tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "name": spec.name,
                    "description": spec.description,
                    "strict": false,
                    "parameters": spec.parameters,
                })
            })
            .collect()
    })
}

/// The reasoning object for a named effort; `None` at the default.
fn reasoning(thinking: ThinkingEffort) -> Option<Reasoning> {
    let effort = match thinking {
        ThinkingEffort::Default => return None,
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
    };
    Some(Reasoning {
        effort,
        summary: "auto",
    })
}

/// One message's text blocks, joined the way the sibling adapters join
/// them.
fn texts(message: &Message) -> String {
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

/// The data URI an image rides.
fn image_url(image: &rhema_core::Image) -> String {
    format!("data:{};base64,{}", image.media_type.as_str(), image.data)
}

/// JSON-encoded arguments; `null` reads as an empty object.
fn arguments_text(args: &Value) -> String {
    if args.is_null() {
        "{}".to_owned()
    } else {
        args.to_string()
    }
}

// --- Mapping: SSE events -> ModelOutput / events ---

/// One streamed tool call being assembled across events.
#[derive(Debug, Default)]
struct PartialCall {
    /// The backend's correlation id.
    call_id: String,
    /// The tool name.
    name: String,
    /// The argument JSON accumulated so far.
    args: String,
    /// Whether `ToolCallStart` has been emitted.
    started: bool,
    /// Whether argument deltas have been emitted.
    streamed: bool,
}

/// One SSE frame's fields. Events carry many more; unknown ones are
/// ignored.
#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    response: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    /// The top-level message a stream `error` event carries.
    #[serde(default)]
    message: Option<String>,
}

/// Assembles one Responses SSE stream into a [`ModelOutput`] and
/// emits the streaming events on the way.
#[derive(Debug, Default)]
pub struct StreamAssembler {
    /// Text deltas as they arrived.
    text: String,
    /// The terminal event's text, when it carried any.
    final_text: Option<String>,
    /// Calls by key (the call id), in first-seen order via `order`.
    calls: BTreeMap<String, PartialCall>,
    /// First-seen call keys, in order.
    order: Vec<String>,
    /// `item_id` -> call key, for argument deltas.
    item_index: HashMap<String, String>,
    /// The last usage frame.
    usage: Usage,
    /// The terminal stop reason.
    stop: Option<StopReason>,
    /// The synthetic-id counter for calls that arrive without one.
    next_synthetic: usize,
}

impl StreamAssembler {
    /// An empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one SSE payload in, emitting the events it carries.
    ///
    /// # Errors
    ///
    /// A malformed frame, a `response.failed`, a terminal `error`, or a
    /// content-filtered incomplete response is an
    /// [`ProviderError::Api`] without a status.
    pub fn push(&mut self, payload: &str, sink: &mut dyn EventSink) -> Result<(), ProviderError> {
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return Ok(());
        }
        let event: StreamEvent = serde_json::from_str(payload)
            .map_err(|err| stream_error(format!("malformed ChatGPT SSE chunk: {err}")))?;
        match event.kind.as_str() {
            "response.output_item.added" => {
                if let Some(item) = event.item.as_ref() {
                    self.on_item(item, sink, false);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.item.as_ref() {
                    self.on_item(item, sink, true);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = event.delta.filter(|delta| !delta.is_empty()) {
                    self.text.push_str(&delta);
                    sink.emit(Event::TextDelta(&delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.delta.filter(|delta| !delta.is_empty()) {
                    sink.emit(Event::ThinkingDelta(&delta));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta.filter(|delta| !delta.is_empty()) {
                    self.on_arguments_delta(
                        event.item_id.as_deref(),
                        event.call_id.as_deref(),
                        &delta,
                        sink,
                    );
                }
            }
            "response.completed" => {
                self.on_terminal(event.response, sink, false)?;
            }
            "response.incomplete" => {
                self.on_terminal(event.response, sink, true)?;
            }
            "response.failed" => {
                return Err(response_error(event.response.as_ref()));
            }
            "error" => {
                // The Responses stream error event puts its message at
                // the top level; an OpenAI-shaped nested error is also
                // accepted.
                let message = event
                    .error
                    .as_ref()
                    .and_then(error_message_of)
                    .or(event.message)
                    .unwrap_or_else(|| "the ChatGPT stream reported an error".to_owned());
                return Err(stream_error(message));
            }
            _ => {}
        }
        Ok(())
    }

    /// The assembled output. Call after the stream ends; a stream that
    /// closed before its terminal event still returns what it streamed.
    pub fn finish(self) -> ModelOutput {
        let text = self
            .final_text
            .filter(|text| !text.is_empty())
            .unwrap_or(self.text);
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text(text));
        }
        for key in &self.order {
            let Some(call) = self.calls.get(key) else {
                continue;
            };
            // A name that never arrived leaves no callable tool; the
            // transcript must not carry a half-call.
            if call.name.is_empty() {
                continue;
            }
            content.push(Content::ToolCall(ToolCall {
                id: call.call_id.clone(),
                name: call.name.clone(),
                args: parse_arguments(&call.args),
                provider_data: Value::Null,
            }));
        }
        let has_calls = content
            .iter()
            .any(|block| matches!(block, Content::ToolCall(_)));
        let stop_reason = if has_calls {
            StopReason::ToolUse
        } else {
            match self.stop {
                // A call that never got a name is not in the message,
                // so a terminal ToolUse verdict has nothing to back it.
                Some(StopReason::ToolUse) | None => StopReason::EndTurn,
                Some(other) => other,
            }
        };
        ModelOutput {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: self.usage,
        }
    }

    /// Handles one `output_item.added`/`.done` item.
    fn on_item(&mut self, item: &Value, sink: &mut dyn EventSink, done: bool) {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let key = self.ensure_call(item);
                self.start_call(&key, sink);
                if done {
                    let full = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|args| !args.is_empty());
                    if let Some(full) = full
                        && let Some(call) = self.calls.get_mut(&key)
                    {
                        call.args = full.to_owned();
                    }
                    self.stream_full_args(&key, sink);
                }
            }
            Some("message") if done => {
                // Deltas are the normal path; a stream that only sent
                // the final item still gets its text.
                if self.text.is_empty()
                    && self.final_text.is_none()
                    && let Some(text) = message_text(item)
                {
                    sink.emit(Event::TextDelta(&text));
                    self.final_text = Some(text);
                }
            }
            _ => {}
        }
    }

    /// Handles one argument fragment.
    fn on_arguments_delta(
        &mut self,
        item_id: Option<&str>,
        call_id: Option<&str>,
        delta: &str,
        sink: &mut dyn EventSink,
    ) {
        let key = self.call_key(item_id, call_id);
        let call = self.calls.entry(key.clone()).or_default();
        if call.call_id.is_empty() {
            call.call_id = call_id
                .map(str::to_owned)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| key.clone());
        }
        call.args.push_str(delta);
        call.streamed = true;
        let id = call.call_id.clone();
        sink.emit(Event::ToolCallArgsDelta { id: &id, delta });
    }

    /// Finds or creates the call an item names, recording the
    /// `item_id` alias.
    fn ensure_call(&mut self, item: &Value) -> String {
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|id| !id.is_empty());
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        // An item seen streaming under one alias keeps that key when
        // the terminal frame fills in its real call id: re-keying would
        // leave a duplicate half-call in the transcript.
        let key = item_id
            .as_ref()
            .and_then(|id| self.item_index.get(id).cloned())
            .or_else(|| call_id.clone())
            .unwrap_or_else(|| {
                self.next_synthetic += 1;
                format!("call_{}", self.next_synthetic)
            });
        let call = self.calls.entry(key.clone()).or_default();
        if call.call_id.is_empty() {
            call.call_id = key.clone();
        }
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            call.call_id = call_id;
        }
        if let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            call.name = name.to_owned();
        }
        if let Some(args) = item
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|args| !args.is_empty())
        {
            call.args = args.to_owned();
        }
        if !self.order.contains(&key) {
            self.order.push(key.clone());
        }
        if let Some(item_id) = item_id {
            self.item_index.insert(item_id, key.clone());
        }
        key
    }

    /// Emits `ToolCallStart` for a call that has a name and has not
    /// started yet.
    fn start_call(&mut self, key: &str, sink: &mut dyn EventSink) {
        let Some(call) = self.calls.get(key) else {
            return;
        };
        if call.started || call.name.is_empty() {
            return;
        }
        let id = call.call_id.clone();
        let name = call.name.clone();
        if let Some(call) = self.calls.get_mut(key) {
            call.started = true;
        }
        sink.emit(Event::ToolCallStart {
            id: &id,
            name: &name,
        });
    }

    /// Emits a call's full arguments as one fragment when no fragment
    /// streamed for it, which is the non-streaming shape.
    fn stream_full_args(&mut self, key: &str, sink: &mut dyn EventSink) {
        let Some(call) = self.calls.get(key) else {
            return;
        };
        if call.streamed || call.args.is_empty() {
            return;
        }
        let id = call.call_id.clone();
        let args = call.args.clone();
        if let Some(call) = self.calls.get_mut(key) {
            call.streamed = true;
        }
        sink.emit(Event::ToolCallArgsDelta {
            id: &id,
            delta: &args,
        });
    }

    /// Handles the terminal `response.completed`/`.incomplete` event.
    fn on_terminal(
        &mut self,
        response: Option<Value>,
        sink: &mut dyn EventSink,
        incomplete: bool,
    ) -> Result<(), ProviderError> {
        let Some(response) = response else {
            return Err(stream_error(
                "ChatGPT stream ended without a response object".to_owned(),
            ));
        };
        if response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(response_error(Some(&response)));
        }
        let reason = response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str);
        if incomplete && reason == Some("content_filter") {
            return Err(stream_error(
                "the ChatGPT response was filtered by the safety system".to_owned(),
            ));
        }

        // The terminal output is authoritative: it carries the calls
        // the backend actually accepted, complete arguments included.
        let mut terminal_calls = Vec::new();
        let mut terminal_text = String::new();
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            for item in output {
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        if let Some(text) = message_text(item) {
                            if !terminal_text.is_empty() {
                                terminal_text.push_str("\n\n");
                            }
                            terminal_text.push_str(&text);
                        }
                    }
                    Some("function_call") => {
                        terminal_calls.push(self.ensure_call(item));
                    }
                    _ => {}
                }
            }
        }
        if !terminal_text.is_empty() {
            // A stream that only sent the terminal output still gets
            // its text on the sink.
            if self.text.is_empty() && self.final_text.is_none() {
                sink.emit(Event::TextDelta(&terminal_text));
            }
            self.final_text = Some(terminal_text);
        }
        for key in &terminal_calls {
            self.start_call(key, sink);
            self.stream_full_args(key, sink);
        }

        self.usage = usage_of(&response);
        self.stop = Some(stop_reason(incomplete, reason, &self.calls));
        sink.emit(Event::Usage(self.usage));
        Ok(())
    }

    /// The key a fragment belongs to: the item alias when known, else
    /// the event's call id, else a fresh synthetic key.
    fn call_key(&mut self, item_id: Option<&str>, call_id: Option<&str>) -> String {
        if let Some(item_id) = item_id
            && let Some(key) = self.item_index.get(item_id)
        {
            return key.clone();
        }
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            return call_id.to_owned();
        }
        self.next_synthetic += 1;
        format!("call_{}", self.next_synthetic)
    }
}

/// A message item's joined `output_text` content.
fn message_text(item: &Value) -> Option<String> {
    let text = item
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            // A refusal is assistant text the user must see.
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

/// The stop reason the terminal event names.
fn stop_reason(
    incomplete: bool,
    reason: Option<&str>,
    calls: &BTreeMap<String, PartialCall>,
) -> StopReason {
    if !calls.is_empty() {
        return StopReason::ToolUse;
    }
    if !incomplete {
        return StopReason::EndTurn;
    }
    match reason {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("interrupted") => StopReason::Other("interrupted".to_owned()),
        Some(other) => StopReason::Other(other.to_owned()),
        None => StopReason::Other("incomplete".to_owned()),
    }
}

/// The terminal response's usage object.
fn usage_of(response: &Value) -> Usage {
    let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) else {
        return Usage::default();
    };
    Usage {
        input_tokens: u32_of(usage.get("input_tokens")),
        output_tokens: u32_of(usage.get("output_tokens")),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    }
}

/// A JSON number as `u32`, saturating.
fn u32_of(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

/// Parses a call's accumulated arguments, keeping malformed text
/// instead of failing the turn.
fn parse_arguments(args: &str) -> Value {
    if args.is_empty() {
        return json!({});
    }
    serde_json::from_str(args).unwrap_or_else(|_| json!({ "_chatgpt_args_text": args }))
}

/// The API-level error a failed response carries.
fn response_error(response: Option<&Value>) -> ProviderError {
    let error = response
        .and_then(|response| response.get("error"))
        .filter(|error| !error.is_null())
        .or_else(|| response.and_then(|response| response.get("message")));
    match error {
        Some(error) => error_value(Some(error)),
        None => stream_error("the ChatGPT response failed".to_owned()),
    }
}

/// The message an error value carries, whatever shape it took.
fn error_value(error: Option<&Value>) -> ProviderError {
    stream_error(
        error
            .and_then(error_message_of)
            .unwrap_or_else(|| "the ChatGPT stream reported an error".to_owned()),
    )
}

/// The message inside an error value, whatever shape it took.
fn error_message_of(error: &Value) -> Option<String> {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_owned)
}

/// A mid-stream protocol error: the HTTP call succeeded, so no status.
fn stream_error(message: String) -> ProviderError {
    ProviderError::Api {
        message: format!("chatgpt api error: {message}"),
        status: None,
    }
}

// --- Mapping: the models page -> ModelCard ---

/// Normalizes the ChatGPT backend's `{"models": […]}` page: visible,
/// API-servable rows become chat cards.
///
/// A row's `slug` is the id, `context_window` (or `max_context_window`)
/// its input ceiling, and `input_modalities` decides the vision flag
/// when it names one. Sparse rows default rather than fail: a catalog
/// drift must not take the picker down.
pub fn normalize_models(page: &Value) -> Vec<ModelCard> {
    let entries = page
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .filter(|entry| {
            entry
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility != "hide")
        })
        .filter(|entry| {
            entry
                .get("supported_in_api")
                .and_then(Value::as_bool)
                .is_none_or(|supported| supported)
        })
        .filter_map(|entry| {
            let id = entry.get("slug").and_then(Value::as_str)?.to_owned();
            let display_name = entry
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(&id)
                .to_owned();
            let description = entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let input_token_limit = entry
                .get("context_window")
                .and_then(Value::as_u64)
                .or_else(|| entry.get("max_context_window").and_then(Value::as_u64));
            let output_token_limit = entry.get("max_output_tokens").and_then(Value::as_u64);
            let supports_vision =
                entry
                    .get("input_modalities")
                    .and_then(Value::as_array)
                    .map(|modalities| {
                        modalities
                            .iter()
                            .filter_map(Value::as_str)
                            .any(|modality| modality == "image")
                    });
            Some(ModelCard {
                id,
                display_name,
                description,
                input_token_limit,
                output_token_limit,
                supports_vision,
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::message::Image;
    use rhema_core::request::ToolSpec;

    fn request() -> ModelRequest {
        ModelRequest {
            system: Some("be terse".into()),
            messages: vec![Message::text(Role::User, "say hello")],
            tools: vec![],
            params: ChatParams::new("gpt-5.1-codex"),
        }
    }

    use rhema_core::request::ChatParams;

    fn events(output: &ModelOutput) -> Vec<String> {
        output
            .message
            .content
            .iter()
            .map(|content| match content {
                Content::Text(text) => format!("text:{text}"),
                Content::ToolCall(call) => {
                    format!("call:{}:{}:{}", call.id, call.name, call.args)
                }
                other => format!("other:{other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_system_prompt_and_user_turn_map_to_instructions_and_items() {
        let body = request_body(&request());
        assert_eq!(body.instructions.as_deref(), Some("be terse"));
        assert_eq!(body.model, "gpt-5.1-codex");
        assert_eq!(
            body.input,
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "say hello"}],
            })]
        );
        assert!(body.tools.is_none());
        assert!(body.reasoning.is_none());
    }

    #[test]
    fn system_messages_fold_into_instructions() {
        let mut req = request();
        req.messages = vec![
            Message::text(Role::System, "rule one"),
            Message::text(Role::User, "go"),
            Message::text(Role::System, "rule two"),
        ];
        let body = request_body(&req);
        assert_eq!(
            body.instructions.as_deref(),
            Some("be terse\n\nrule one\n\nrule two")
        );
        // Only the user turn remains in input.
        assert_eq!(body.input.len(), 1);
    }

    #[test]
    fn images_ride_data_uris_and_tool_results_keep_their_call_id() {
        let mut req = request();
        req.messages = vec![
            Message::text_with_images(
                Role::User,
                "look",
                vec![Image {
                    media_type: rhema_core::message::ImageMediaType::Png,
                    data: "aGk=".into(),
                }],
            ),
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(rhema_core::tool::ToolResult {
                    id: "call_1".into(),
                    content: "the file".into(),
                    is_error: false,
                    code: None,
                    images: vec![Image {
                        media_type: rhema_core::message::ImageMediaType::Jpeg,
                        data: "aGk=".into(),
                    }],
                })],
            },
        ];
        let body = request_body(&req);
        assert_eq!(
            body.input[0]["content"][1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,aGk="})
        );
        assert_eq!(body.input[1]["type"], "function_call_output");
        assert_eq!(body.input[1]["call_id"], "call_1");
        assert_eq!(body.input[1]["output"][0]["type"], "input_text");
        assert_eq!(body.input[1]["output"][1]["type"], "input_image");
    }

    #[test]
    fn assistant_text_and_tool_calls_replay_on_the_wire() {
        let mut req = request();
        req.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("thinking out loud".into()),
                Content::ToolCall(ToolCall {
                    id: "call_9".into(),
                    name: "bash".into(),
                    args: json!({"cmd": "ls"}),
                    provider_data: json!({"opaque": true}),
                }),
            ],
        }];
        let body = request_body(&req);
        assert_eq!(body.input[0]["content"][0]["type"], "output_text");
        assert_eq!(body.input[1]["type"], "function_call");
        assert_eq!(body.input[1]["call_id"], "call_9");
        assert_eq!(body.input[1]["name"], "bash");
        assert_eq!(body.input[1]["arguments"], r#"{"cmd":"ls"}"#);
    }

    #[test]
    fn tool_specs_use_the_responses_function_shape() {
        let mut req = request();
        req.tools = vec![ToolSpec::new(
            "bash",
            "run a command",
            json!({"type": "object", "properties": {}}),
        )];
        let body = request_body(&req);
        assert_eq!(
            body.tools.unwrap()[0],
            json!({
                "type": "function",
                "name": "bash",
                "description": "run a command",
                "strict": false,
                "parameters": {"type": "object", "properties": {}},
            })
        );
    }

    #[test]
    fn a_named_effort_sends_reasoning_but_the_default_does_not() {
        let mut req = request();
        req.params.thinking = ThinkingEffort::High;
        let body = request_body(&req);
        assert_eq!(body.reasoning.unwrap().effort, "high");
        assert_eq!(reasoning(ThinkingEffort::Low).unwrap().effort, "low");
        assert!(reasoning(ThinkingEffort::Default).is_none());
    }

    #[test]
    fn a_streamed_turn_assembles_text_calls_and_usage() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        let frames = [
            r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"hmm"}"#,
            r#"{"type":"response.output_text.delta","delta":"lo"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"cmd\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]},{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}],"usage":{"input_tokens":7,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}}}"#,
        ];
        for frame in frames {
            assembler.push(frame, &mut sink).expect("the frame parses");
        }
        let output = assembler.finish();
        assert_eq!(
            events(&output),
            vec![
                "text:Hello".to_owned(),
                r#"call:call_1:bash:{"cmd":"ls"}"#.to_owned(),
            ]
        );
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(
            output.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 3,
                cached_input_tokens: 2,
            }
        );
        assert_eq!(
            sink.0,
            vec![
                "TextDelta:Hel".to_owned(),
                "ThinkingDelta:hmm".to_owned(),
                "TextDelta:lo".to_owned(),
                "ToolCallStart:call_1:bash".to_owned(),
                "Args:call_1:{\"cmd\":".to_owned(),
                "Args:call_1:\"ls\"}".to_owned(),
                "Usage:7:3:2".to_owned(),
            ]
        );
    }

    #[test]
    fn the_terminal_output_replaces_the_streamed_deltas() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        assembler
            .push(
                r#"{"type":"response.output_text.delta","delta":"partial"}"#,
                &mut sink,
            )
            .unwrap();
        assembler
            .push(
                r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final"}]}]}}"#,
                &mut sink,
            )
            .unwrap();
        let output = assembler.finish();
        assert_eq!(output.message.content, vec![Content::Text("final".into())]);
        assert_eq!(output.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn arguments_that_never_streamed_emit_once_from_the_done_item() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        assembler
            .push(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"bash","arguments":"{\"cmd\":\"pwd\"}"}}"#,
                &mut sink,
            )
            .unwrap();
        assert_eq!(
            sink.0,
            vec![
                "ToolCallStart:c1:bash".to_owned(),
                "Args:c1:{\"cmd\":\"pwd\"}".to_owned(),
            ]
        );
        let output = assembler.finish();
        assert_eq!(output.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn malformed_arguments_survive_as_text() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        assembler
            .push(
                r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"c1","name":"bash","arguments":"{oops"}]}}"#,
                &mut sink,
            )
            .unwrap();
        let output = assembler.finish();
        let Content::ToolCall(call) = &output.message.content[0] else {
            panic!("the call is there");
        };
        assert_eq!(call.args, json!({"_chatgpt_args_text": "{oops"}));
    }

    #[test]
    fn a_failed_response_is_an_api_error_without_a_status() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        let err = assembler
            .push(
                r#"{"type":"response.failed","response":{"error":{"message":"quota exhausted"}}}"#,
                &mut sink,
            )
            .expect_err("the failure surfaces");
        assert_eq!(
            err,
            ProviderError::Api {
                message: "chatgpt api error: quota exhausted".into(),
                status: None,
            }
        );
    }

    #[test]
    fn a_top_level_error_event_surfaces() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        let err = assembler
            .push(
                r#"{"type":"error","error":{"message":"bad request"}}"#,
                &mut sink,
            )
            .expect_err("the error surfaces");
        assert!(format!("{err}").contains("bad request"), "{err}");
    }

    #[test]
    fn a_top_level_stream_error_keeps_its_message() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        let err = assembler
            .push(
                r#"{"type":"error","code":"server_error","message":"upstream overloaded"}"#,
                &mut sink,
            )
            .expect_err("the error surfaces");
        assert!(format!("{err}").contains("upstream overloaded"), "{err}");
    }

    #[test]
    fn max_output_tokens_maps_to_the_token_ceiling() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        assembler
            .push(
                r#"{"type":"response.incomplete","response":{"status":"incomplete","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"half"}]}],"incomplete_details":{"reason":"max_output_tokens"}}}"#,
                &mut sink,
            )
            .unwrap();
        let output = assembler.finish();
        assert_eq!(output.stop_reason, StopReason::MaxTokens);
        assert_eq!(output.message.content, vec![Content::Text("half".into())]);
    }

    #[test]
    fn a_content_filtered_response_is_an_error() {
        let mut assembler = StreamAssembler::new();
        let mut sink = Collector::default();
        assert!(
            assembler
                .push(
                    r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
                    &mut sink,
                )
                .is_err()
        );
    }

    #[test]
    fn the_models_page_keeps_visible_api_rows() {
        let page = json!({
            "models": [
                {
                    "slug": "gpt-5.1-codex",
                    "display_name": "GPT-5.1 Codex",
                    "description": "Codex",
                    "context_window": 400000,
                    "max_context_window": 500000,
                    "input_modalities": ["text", "image"],
                    "visibility": "list",
                    "supported_in_api": true
                },
                {"slug": "hidden", "visibility": "hide", "supported_in_api": true},
                {"slug": "not-api", "supported_in_api": false},
            ]
        });
        let models = normalize_models(&page);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.1-codex");
        assert_eq!(models[0].input_token_limit, Some(400_000));
        assert_eq!(models[0].supports_vision, Some(true));
        assert!(models[0].supports_chat());
        assert_eq!(models[0].pricing, None);
    }

    /// Records events as compact strings.
    #[derive(Default)]
    struct Collector(Vec<String>);

    impl EventSink for Collector {
        fn emit(&mut self, event: Event<'_>) {
            match event {
                Event::TextDelta(delta) => self.0.push(format!("TextDelta:{delta}")),
                Event::ThinkingDelta(delta) => self.0.push(format!("ThinkingDelta:{delta}")),
                Event::ToolCallStart { id, name } => {
                    self.0.push(format!("ToolCallStart:{id}:{name}"));
                }
                Event::ToolCallArgsDelta { id, delta } => {
                    self.0.push(format!("Args:{id}:{delta}"));
                }
                Event::Usage(usage) => self.0.push(format!(
                    "Usage:{}:{}:{}",
                    usage.input_tokens, usage.output_tokens, usage.cached_input_tokens
                )),
                other => self.0.push(format!("{other:?}")),
            }
        }
    }
}
