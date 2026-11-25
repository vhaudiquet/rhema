//! Gemini wire types and the pure mapping between them and rhema-core's
//! model. No I/O here, [`crate::provider`] drives these functions.
//!
//! Mapping constraints (Gemini has no notion of rhema-core's richer roles):
//! - Only `user` and `model` roles exist on the wire. `Role::Assistant` maps
//!   to `model`; `Role::User` and `Role::Tool` map to `user`, with tool
//!   results carried as `functionResponse` parts (matched by tool *name*,
//!   never by id). A result's images (`ToolResult::images`) have no wire
//!   home in a function response, it is text, so each is named in a
//!   trailing note.
//! - `Role::System` messages and `ModelRequest.system` fold together into a
//!   single `systemInstruction` text, joined by a blank line.
//! - Gemini tool-call ids are optional on the wire: when the model sends one
//!   it is used verbatim, otherwise a deterministic `call_{index}` is
//!   synthesized from the position of the call among the function calls of
//!   the assembled message. Synthesized ids exist only to satisfy
//!   rhema-core's `ToolCall.id`/`ToolResult.id` round-trip, Gemini ignores
//!   them and matches responses by name.
//! - Thinking models emit `thought` parts (thought summaries) ahead of the
//!   `functionCall` parts they produced, and the API requires the calls'
//!   `thoughtSignature` on replay. Both round-trip through
//!   `ToolCall::provider_data`: the signature under `thoughtSignature`, and
//!   the thought streamed immediately before the call under `thought`,
//!   replayed as a `thought` part ahead of the call so the model continues
//!   its reasoning. Thoughts with no following call stay stream-only.
//! - `functionCall.args` must be a JSON object. Non-object history args are
//!   wrapped as `{"_rhema_args": <original>}`; missing args become `{}`.
//! - `functionResponse.response` must be a JSON object; a successful
//!   `ToolResult` maps to `{"output": <content>}`,
//!   a failed one to `{"error": <content>}`.
//! - Images in a user turn ride one `inlineData` part per image, the
//!   media type and the bare base64 bytes, beside the text parts in the
//!   order the user arranged them. The model never emits images, so the
//!   response side of the mapping is untouched.
//! - Errors: a non-2xx body is `{"error": {code, message, status}}`; the
//!   mapper keeps `error.message` and falls back to the raw body. The
//!   stream can also fail mid-turn with a `data: {"error": ...}` chunk
//!   (then closing it), which surfaces as the same
//!   [`ProviderError::Api`].
//! - Thinking rides one of two knobs: Gemini 3 and later take
//!   `thinkingLevel`, a named depth, while 2.5 and earlier take
//!   `thinkingBudget`, a token count. [`ThinkingStyle`] picks from the
//!   model id, and the provider retries the other knob when the server
//!   rejects the one the name implied. `gemini-3-pro-preview` is the one
//!   documented level family without a `MEDIUM`, so that level maps to
//!   `HIGH` there.

use std::collections::HashMap;

use rhema_core::event::{Event, EventSink};
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::{ModelOutput, StopReason, Usage};
use rhema_core::provider::ProviderError;
use rhema_core::request::{ModelRequest, ThinkingEffort};
use rhema_core::tool::{ToolCall, ToolResult};
use rhema_provider::ModelCard;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker key wrapping non-object `functionCall.args` into a valid object.
const WRAPPED_ARGS_KEY: &str = "_rhema_args";

// --- Request wire types (serialize; camelCase per the API's JSON schema) ---

/// The `models/{model}:streamGenerateContent` request body.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    /// Conversation turns; roles are only `user` and `model`.
    pub contents: Vec<WireContent>,
    /// The folded system prompt, when the request has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<WireContent>,
    /// Native function declarations, when tools are offered this step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
    /// [`rhema_core::ChatParams`] mapped onto the generation config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<WireGenerationConfig>,
}

/// One turn on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireContent {
    /// `"user"` or `"model"`; absent on `systemInstruction`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Ordered parts of the turn.
    pub parts: Vec<WirePart>,
}

/// One part of a [`WireContent`].
///
/// Unknown part kinds (code execution, video, ...) deserialize to an empty
/// part and are dropped by the mapping functions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WirePart {
    /// Text of a text part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present on an inline-image part (requests only): the image's media
    /// type and base64 bytes. Models never emit images, so a response part
    /// never carries it and one that somehow did is dropped as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<WireInlineData>,
    /// `true` on internal thought parts: they stream as
    /// [`Event::ThinkingDelta`], and only the thought that led to a
    /// function call also maps into the message, riding the call's
    /// `provider_data` so replay can put it back ahead of the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    /// Present on a function-call part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<WireFunctionCall>,
    /// Present on a function-response part (requests only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_response: Option<WireFunctionResponse>,
    /// Thinking-model marker attached to function calls: the API rejects
    /// history replays that drop it, so it round-trips through
    /// `ToolCall::provider_data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// The `inlineData` object of an inline-image [`WirePart`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WireInlineData {
    /// The image's IANA media type (e.g. `image/png`).
    #[serde(default)]
    pub mime_type: String,
    /// The image bytes, bare base64, no `data:` prefix, no newlines.
    #[serde(default)]
    pub data: String,
}

/// A `functionCall` part: the model requesting a tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionCall {
    /// Provider-issued id, when the model sent one; otherwise the mapper
    /// synthesizes `call_{index}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Name of the called tool.
    #[serde(default)]
    pub name: String,
    /// Call arguments; must be a JSON object on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// A `functionResponse` part: a tool result sent back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionResponse {
    /// Name of the answered tool, Gemini matches by name, not by id.
    #[serde(default)]
    pub name: String,
    /// Structured payload; must be a JSON object.
    #[serde(default)]
    pub response: Value,
}

/// A `tools[]` entry; rhema offers exactly one, the function declarations.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTool {
    /// The advertised functions.
    pub function_declarations: Vec<WireFunctionDeclaration>,
}

/// One advertised tool.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFunctionDeclaration {
    /// Name the model uses to call the tool.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The tool's argument schema as full JSON Schema, via the API's
    /// `parametersJsonSchema` field (rather than the OpenAPI-subset
    /// `parameters` field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters_json_schema: Option<Value>,
}

/// The `generationConfig` request field.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireGenerationConfig {
    /// Sampling temperature, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus-sampling threshold, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Output token ceiling, when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Stop sequences, when any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Thinking controls, when the driver named a level: one of the two
    /// knobs the model speaks, and the thoughts requested so they stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<WireThinkingConfig>,
}

/// The `thinkingConfig` request field: a reasoning depth and the thought
/// summary turned on, so the thinking streams as it happens. The depth is
/// a named level on Gemini 3 and later, a token budget on 2.5 and
/// earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireThinkingConfig {
    /// The reasoning token budget, on the budget form only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// The reasoning depth (`"LOW"`, `"MEDIUM"`, `"HIGH"`), on the level
    /// form only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<&'static str>,
    /// Always `true`: thought summaries stream as [`Event::ThinkingDelta`].
    pub include_thoughts: bool,
}

/// Which thinking knob a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStyle {
    /// Gemini 3 and later: `thinkingLevel`, a named depth. Earlier models
    /// reject the field.
    Level,
    /// Gemini 2.5 and earlier: `thinkingBudget`, a token count. Gemini 3
    /// replaced it.
    Budget,
}

impl ThinkingStyle {
    /// The knob `model` speaks. Gemini 3 and later take the named level;
    /// 2.5 and earlier the budget. An id with no Gemini version keeps the
    /// budget, the knob that shipped.
    pub fn for_model(model: &str) -> Self {
        let model = model.trim();
        let model = model.strip_prefix("models/").unwrap_or(model);
        let rest = model.strip_prefix("gemini-").unwrap_or(model);
        // A version component is at most two digits; a longer run is a
        // date, not a major version.
        let major = rest
            .split(['-', '.'])
            .filter_map(|part| part.parse::<u32>().ok())
            .find(|number| *number <= 99);
        match major {
            Some(major) if major >= 3 => Self::Level,
            _ => Self::Budget,
        }
    }

    /// The other knob: what a request retries with when the server
    /// rejects this one for its model.
    pub fn other(self) -> Self {
        match self {
            Self::Level => Self::Budget,
            Self::Budget => Self::Level,
        }
    }
}

/// The reasoning budget a thinking level maps onto, on the budget form.
fn thinking_budget(thinking: ThinkingEffort) -> Option<u32> {
    match thinking {
        ThinkingEffort::Default => None,
        ThinkingEffort::Low => Some(4_096),
        ThinkingEffort::Medium => Some(16_384),
        ThinkingEffort::High => Some(32_768),
    }
}

/// The depth a thinking level maps onto, on the level form. The one
/// documented family without a `MEDIUM`, `gemini-3-pro-preview`, takes
/// `HIGH` for it.
fn thinking_level(thinking: ThinkingEffort, model: &str) -> Option<&'static str> {
    match thinking {
        ThinkingEffort::Default => None,
        ThinkingEffort::Low => Some("LOW"),
        ThinkingEffort::Medium if is_gemini_3_pro(model) => Some("HIGH"),
        ThinkingEffort::Medium => Some("MEDIUM"),
        ThinkingEffort::High => Some("HIGH"),
    }
}

/// Whether `model` is the one documented level family whose set skips
/// `MEDIUM`: `gemini-3-pro-preview` and its aliases, but not the 3.1
/// refresh, which added the level.
fn is_gemini_3_pro(model: &str) -> bool {
    let model = model.trim();
    let model = model.strip_prefix("models/").unwrap_or(model);
    model.starts_with("gemini-3-pro")
}

/// The thinking config for `thinking` in `style`'s form, `None` at the
/// default.
fn thinking_config(
    thinking: ThinkingEffort,
    style: ThinkingStyle,
    model: &str,
) -> Option<WireThinkingConfig> {
    if thinking == ThinkingEffort::Default {
        return None;
    }
    Some(match style {
        ThinkingStyle::Budget => WireThinkingConfig {
            thinking_budget: thinking_budget(thinking),
            thinking_level: None,
            include_thoughts: true,
        },
        ThinkingStyle::Level => WireThinkingConfig {
            thinking_budget: None,
            thinking_level: thinking_level(thinking, model),
            include_thoughts: true,
        },
    })
}

// --- Response wire types (deserialize) ---

/// One `GenerateContentResponse`: a full response, or one SSE chunk, or,
/// on a stream aborted by a failure, the error object the API sends in
/// place of one.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    /// Response candidates; streaming chunks typically carry zero or one.
    #[serde(default)]
    pub candidates: Vec<WireCandidate>,
    /// Token accounting; usually only on the last chunk.
    #[serde(default)]
    pub usage_metadata: Option<WireUsageMetadata>,
    /// Prompt-level block feedback, when the request was refused outright.
    #[serde(default)]
    pub prompt_feedback: Option<WirePromptFeedback>,
    /// The API's error detail, on a stream aborted by a failure: the
    /// payload body is the whole `{"error": {...}}` wrapper, so this
    /// field holds the detail directly and stays `None` on every normal
    /// chunk.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// One response candidate.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireCandidate {
    /// The model's content for this candidate.
    #[serde(default)]
    pub content: Option<WireContent>,
    /// Why generation stopped; may be absent on intermediate chunks.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The `usageMetadata` response field.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireUsageMetadata {
    /// Prompt tokens billed.
    #[serde(default)]
    pub prompt_token_count: Option<u32>,
    /// Output tokens billed.
    #[serde(default)]
    pub candidates_token_count: Option<u32>,
    /// The reasoning tokens a thinking model billed as output.
    #[serde(default)]
    pub thoughts_token_count: Option<u32>,
    /// Prompt subset served from the provider's cache.
    #[serde(default)]
    pub cached_content_token_count: Option<u32>,
}

/// The `promptFeedback` response field.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePromptFeedback {
    /// Why the prompt was blocked, when it was.
    #[serde(default)]
    pub block_reason: Option<String>,
}

/// The `error` object of a non-2xx response body.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireErrorBody {
    /// The error detail, when the body has the documented shape.
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

/// Google API error detail: `{"code": ..., "message": ..., "status": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireErrorDetail {
    /// HTTP status code.
    #[serde(default)]
    pub code: Option<i32>,
    /// Human-readable description.
    #[serde(default)]
    pub message: Option<String>,
    /// gRPC-style status (e.g. `INVALID_ARGUMENT`).
    #[serde(default)]
    pub status: Option<String>,
    /// Typed detail records, when the body explains its failure; a
    /// quota reset rides one of the `RetryInfo` kind.
    #[serde(default)]
    pub details: Vec<WireErrorTypedDetail>,
}

/// One typed record of an error body's `details` array. Google folds
/// its rpc helpers there, each discriminated by `@type`; the one this
/// crate reads is
/// `type.googleapis.com/google.rpc.RetryInfo`, whose `retryDelay`
/// names how long the caller should wait.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireErrorTypedDetail {
    /// The record's type discriminator (`@type`).
    #[serde(rename = "@type", default)]
    pub kind: Option<String>,
    /// A `RetryInfo` record's `retryDelay`, a proto duration string
    /// (`"30s"`, `"5.5s"`) when the record is one.
    #[serde(default)]
    pub retry_delay: Option<String>,
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

    /// The quota reset a `RetryInfo` detail names, its `30s`-style
    /// `retryDelay`, the one 429 source the error body itself carries.
    pub fn retry_delay(&self) -> Option<&str> {
        self.error
            .as_ref()?
            .details
            .iter()
            .find(|detail| {
                detail.kind.as_deref() == Some("type.googleapis.com/google.rpc.RetryInfo")
            })
            .and_then(|detail| detail.retry_delay.as_deref())
    }
}

// --- Mapping: ModelRequest -> wire ---

/// Builds the Gemini request body for `req`.
///
/// Fails with [`ProviderError::InvalidRequest`] when the model is unset or
/// the request would produce no contents (Gemini rejects empty `contents`).
pub fn request_body(req: &ModelRequest) -> Result<GenerateContentRequest, ProviderError> {
    request_body_with(req, ThinkingStyle::for_model(&req.params.model))
}

/// Builds the `GenerateContentRequest` for `req`, with the thinking
/// level in `style`'s form. [`request_body`] picks the style from the
/// model; the provider calls this to retry the other form when a server
/// rejects the one the model name implied.
///
/// # Errors
///
/// Fails with [`ProviderError::InvalidRequest`] when the model is unset
/// or the request has no messages to send.
pub fn request_body_with(
    req: &ModelRequest,
    style: ThinkingStyle,
) -> Result<GenerateContentRequest, ProviderError> {
    if req.params.model.trim().is_empty() {
        return Err(ProviderError::InvalidRequest(
            "chat params have no model".into(),
        ));
    }

    let mut system_texts: Vec<String> = Vec::new();
    if let Some(system) = req.system.as_deref()
        && !system.trim().is_empty()
    {
        system_texts.push(system.to_owned());
    }
    let mut contents: Vec<WireContent> = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();
    for message in &req.messages {
        if message.role == Role::System {
            system_texts.extend(texts_of(message));
            continue;
        }
        let role = match message.role {
            Role::Assistant => "model",
            // Function responses ride inside a `user` turn: Gemini has no
            // tool role.
            _ => "user",
        };
        let mut parts: Vec<WirePart> = Vec::new();
        for content in &message.content {
            match content {
                Content::Text(text) => parts.push(WirePart {
                    text: Some(text.clone()),
                    ..WirePart::default()
                }),
                Content::ToolCall(call) => {
                    call_names.insert(call.id.clone(), call.name.clone());
                    // The call's thought replays first, in the position the
                    // model generated it: reasoning, then the call.
                    if let Some(thought) = call
                        .provider_data
                        .get("thought")
                        .and_then(Value::as_str)
                        .filter(|thought| !thought.is_empty())
                    {
                        parts.push(WirePart {
                            text: Some(thought.to_owned()),
                            thought: Some(true),
                            ..WirePart::default()
                        });
                    }
                    parts.push(WirePart {
                        thought_signature: thought_signature(&call.provider_data),
                        function_call: Some(WireFunctionCall {
                            // Synthesized rhema-core ids are never echoed;
                            // the model matches by name.
                            id: None,
                            name: call.name.clone(),
                            args: Some(object_args(&call.args)),
                        }),
                        ..WirePart::default()
                    });
                }
                Content::ToolResult(result) => {
                    let name = call_names
                        .get(&result.id)
                        .cloned()
                        .unwrap_or_else(|| result.id.clone());
                    parts.push(WirePart {
                        function_response: Some(WireFunctionResponse {
                            name,
                            response: response_object(result),
                        }),
                        ..WirePart::default()
                    });
                    // The result's images follow in the same user turn as
                    // inline parts: a function response's JSON is text
                    // only, and Gemini takes `inlineData` on a user turn,
                    // the shape a pasted image already rides.
                    for image in &result.images {
                        parts.push(WirePart {
                            inline_data: Some(WireInlineData {
                                mime_type: image.media_type.as_str().to_owned(),
                                data: image.data.clone(),
                            }),
                            ..WirePart::default()
                        });
                    }
                }
                Content::Image(image) => parts.push(WirePart {
                    inline_data: Some(WireInlineData {
                        mime_type: image.media_type.as_str().to_owned(),
                        data: image.data.clone(),
                    }),
                    ..WirePart::default()
                }),
            }
        }
        if parts.is_empty() {
            continue;
        }
        // Consecutive turns with the same wire role (e.g. several tool
        // results, or a tool result followed by user text) merge into one
        // content: the API wants alternating `user`/`model` turns.
        if contents
            .last()
            .is_some_and(|last| last.role.as_deref() == Some(role))
        {
            contents
                .last_mut()
                .expect("checked above")
                .parts
                .extend(parts);
        } else {
            contents.push(WireContent {
                role: Some(role.to_owned()),
                parts,
            });
        }
    }

    if contents.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "request has no messages to send".into(),
        ));
    }

    let system_instruction = (!system_texts.is_empty()).then(|| WireContent {
        role: None,
        parts: vec![WirePart {
            text: Some(system_texts.join("\n\n")),
            ..WirePart::default()
        }],
    });
    let tools = (!req.tools.is_empty()).then(|| {
        vec![WireTool {
            function_declarations: req
                .tools
                .iter()
                .map(|spec| WireFunctionDeclaration {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    // Only object schemas are valid argument schemas.
                    parameters_json_schema: spec
                        .parameters
                        .is_object()
                        .then(|| spec.parameters.clone()),
                })
                .collect(),
        }]
    });
    let thinking_config = thinking_config(req.params.thinking, style, &req.params.model);
    let generation_config = (req.params.temperature.is_some()
        || req.params.top_p.is_some()
        || req.params.max_tokens.is_some()
        || !req.params.stop.is_empty()
        || thinking_config.is_some())
    .then_some(WireGenerationConfig {
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        max_output_tokens: req.params.max_tokens,
        stop_sequences: req.params.stop.clone(),
        thinking_config,
    });

    Ok(GenerateContentRequest {
        contents,
        system_instruction,
        tools,
        generation_config,
    })
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

/// Extracts the thought signature the provider stored in
/// `ToolCall::provider_data` on the way in; `None` when absent.
fn thought_signature(provider_data: &Value) -> Option<String> {
    provider_data
        .get("thoughtSignature")
        .and_then(|value| value.as_str())
        .filter(|signature| !signature.is_empty())
        .map(str::to_owned)
}

/// `functionCall.args` must be a JSON object: objects pass through, absent
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

/// `functionResponse.response` must be a JSON object; the content rides
/// under `output` on success and `error` on failure. A result's images
/// have no wire home there, a function response is text, so they ride
/// the same user turn as inline parts, right after the response part.
fn response_object(result: &ToolResult) -> Value {
    let content = result.content.clone();
    if result.is_error {
        serde_json::json!({ "error": content })
    } else {
        serde_json::json!({ "output": content })
    }
}

// --- Mapping: wire -> ModelOutput / events ---

/// A wire part normalized into rhema-core's content model.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedPart {
    /// Model text.
    Text(String),
    /// A tool call, carrying the id the final output must use (the wire id
    /// when present, else the synthesized `call_{index}`).
    ToolCall(ToolCall),
}

impl NormalizedPart {
    /// Emits the streaming events this part owes the sink: one `TextDelta`
    /// per text part, one `ToolCallStart` plus one `ToolCallArgsDelta` (the
    /// full args serialized) per call, the same contract as
    /// `rhema_core::MockProvider`.
    pub fn emit(&self, sink: &mut dyn EventSink) {
        match self {
            NormalizedPart::Text(text) => sink.emit(Event::TextDelta(text)),
            NormalizedPart::ToolCall(call) => {
                let args = call.args.to_string();
                sink.emit(Event::ToolCallStart {
                    id: &call.id,
                    name: &call.name,
                });
                sink.emit(Event::ToolCallArgsDelta {
                    id: &call.id,
                    delta: &args,
                });
            }
        }
    }

    /// The history block this part becomes.
    pub fn to_content(&self) -> Content {
        match self {
            NormalizedPart::Text(text) => Content::Text(text.clone()),
            NormalizedPart::ToolCall(call) => Content::ToolCall(call.clone()),
        }
    }
}

/// Assigns ids to function calls: the wire id when present, else a
/// deterministic `call_{index}` counting every function call seen so far.
#[derive(Debug, Default)]
pub struct PartNormalizer {
    tool_calls: u32,
}

impl PartNormalizer {
    /// A normalizer that has seen no calls.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalizes one wire part; `None` for thought parts and for parts with
    /// no rhema-mappable content (empty text, unknown kinds).
    pub fn normalize(&mut self, part: &WirePart) -> Option<NormalizedPart> {
        if part.thought == Some(true) {
            return None;
        }
        if let Some(call) = &part.function_call {
            let index = self.tool_calls;
            self.tool_calls += 1;
            let id = match &call.id {
                Some(id) if !id.is_empty() => id.clone(),
                _ => format!("call_{index}"),
            };
            let provider_data = match &part.thought_signature {
                Some(signature) if !signature.is_empty() => {
                    serde_json::json!({ "thoughtSignature": signature })
                }
                _ => Value::Null,
            };
            return Some(NormalizedPart::ToolCall(ToolCall {
                id,
                name: call.name.clone(),
                args: call
                    .args
                    .clone()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
                provider_data,
            }));
        }
        match part.text.as_deref() {
            Some(text) if !text.is_empty() => Some(NormalizedPart::Text(text.to_owned())),
            _ => None,
        }
    }
}

/// Assembles SSE chunks into the final [`ModelOutput`], emitting streaming
/// events as parts arrive.
///
/// `finishReason` and `usageMetadata` may arrive on any chunk (typically the
/// last non-empty one); the last value seen wins. When the assembled message
/// contains function calls, the effective stop reason is
/// [`StopReason::ToolUse`] regardless of the wire value, Gemini reports
/// `STOP` for tool-call turns. A `finishReason` (or a refused prompt's
/// `blockReason`) is the stream's terminal proof; a drain that ends
/// without one refuses the stream instead of standing the half answer
/// up as a result.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    normalizer: PartNormalizer,
    parts: Vec<NormalizedPart>,
    /// The thought text streamed since the last part: claimed by the next
    /// function call for replay, dropped once ordinary text resumes.
    pending_thought: String,
    finish_reason: Option<String>,
    blocked: Option<String>,
    usage: Usage,
}

impl ChunkAssembler {
    /// An assembler that has seen no chunks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one response chunk, emitting its parts' streaming events.
    /// Fails with [`ProviderError::Api`] when the chunk is the API's
    /// mid-stream error object.
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Api`] (no status) when the chunk is
    /// the API's mid-stream error.
    pub fn push(
        &mut self,
        chunk: GenerateContentResponse,
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
        if let Some(usage) = &chunk.usage_metadata {
            self.usage = Usage {
                input_tokens: usage.prompt_token_count.unwrap_or(0),
                // Gemini bills a thinking model's reasoning tokens as
                // output, so the sum is what the bill and every
                // downstream figure sees.
                output_tokens: usage
                    .candidates_token_count
                    .unwrap_or(0)
                    .saturating_add(usage.thoughts_token_count.unwrap_or(0)),
                cached_input_tokens: usage.cached_content_token_count.unwrap_or(0),
            };
            // Gemini reports cumulative usage on every chunk: the live
            // tally is exact here, not an estimate.
            sink.emit(Event::Usage(self.usage));
        }
        if let Some(blocked) = chunk.prompt_feedback.and_then(|f| f.block_reason) {
            self.blocked = Some(blocked);
        }
        if let Some(candidate) = chunk.candidates.into_iter().next() {
            if let Some(finish_reason) = candidate.finish_reason {
                self.finish_reason = Some(finish_reason);
            }
            if let Some(content) = candidate.content {
                for part in &content.parts {
                    // Thought parts stream as reasoning deltas and never
                    // join the assembled message as text (normalize drops
                    // them); the text also accumulates so the function call
                    // it led to can replay it.
                    if part.thought == Some(true) {
                        if let Some(text) = part.text.as_deref().filter(|text| !text.is_empty()) {
                            sink.emit(Event::ThinkingDelta(text));
                            self.pending_thought.push_str(text);
                        }
                        continue;
                    }
                    if let Some(mut normalized) = self.normalizer.normalize(part) {
                        match &mut normalized {
                            // The call claims the thought streamed ahead of
                            // it; the signature already in provider_data
                            // keeps its slot.
                            NormalizedPart::ToolCall(call) => {
                                let thought = std::mem::take(&mut self.pending_thought);
                                if !thought.is_empty() {
                                    if call.provider_data.is_null() {
                                        call.provider_data =
                                            serde_json::json!({ "thought": thought });
                                    } else if let Some(map) = call.provider_data.as_object_mut() {
                                        map.insert("thought".into(), Value::String(thought));
                                    }
                                }
                            }
                            // A text response ends the thought's relevance:
                            // only call-adjacent thoughts replay.
                            NormalizedPart::Text(_) => self.pending_thought.clear(),
                        }
                        normalized.emit(sink);
                        self.parts.push(normalized);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a chunk carried this wire's terminal proof: the last
    /// chunk's `finishReason`, or a `promptFeedback.blockReason` for a
    /// prompt the server refused. The drain checks it before standing
    /// the assembled output up as a result.
    pub fn saw_terminal(&self) -> bool {
        self.finish_reason.is_some() || self.blocked.is_some()
    }

    /// Consumes the assembler into the final output. The message is always
    /// `Role::Assistant`; an empty wire response yields an empty content
    /// list, not an error.
    pub fn finish(self) -> ModelOutput {
        let message = Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: self.parts.iter().map(NormalizedPart::to_content).collect(),
        };
        let stop_reason = match (&self.finish_reason, self.parts.is_empty()) {
            (None, true) if self.blocked.is_some() => StopReason::Other(format!(
                "prompt blocked by {}",
                self.blocked.expect("checked")
            )),
            (finish_reason, _) => stop_reason(finish_reason.as_deref(), &self.parts),
        };
        ModelOutput {
            message,
            stop_reason,
            usage: self.usage,
        }
    }
}

/// Maps the wire `finishReason` onto rhema-core's stop reason; any
/// function call in the assembled message wins as [`StopReason::ToolUse`].
fn stop_reason(finish_reason: Option<&str>, parts: &[NormalizedPart]) -> StopReason {
    if parts
        .iter()
        .any(|part| matches!(part, NormalizedPart::ToolCall(_)))
    {
        return StopReason::ToolUse;
    }
    match finish_reason {
        None | Some("") | Some("FINISH_REASON_UNSPECIFIED") | Some("STOP") => StopReason::EndTurn,
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

// --- ListModels: wire -> normalized ---

/// One page of the ListModels response.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireModelsPage {
    /// The models on this page.
    #[serde(default)]
    pub models: Vec<WireModel>,
    /// Token to fetch the next page, when there is one.
    #[serde(default)]
    pub next_page_token: Option<String>,
}

/// A wire `Model` object; fields documented as optional come through with
/// `#[serde(default)]`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireModel {
    /// Resource name, `models/{model}`.
    #[serde(default)]
    pub name: String,
    /// Human-readable name.
    #[serde(default)]
    pub display_name: String,
    /// What the model is for.
    #[serde(default)]
    pub description: String,
    /// Prompt ceiling in tokens, when advertised.
    #[serde(default)]
    pub input_token_limit: Option<u64>,
    /// Output ceiling in tokens, when advertised.
    #[serde(default)]
    pub output_token_limit: Option<u64>,
    /// The API's `supportedGenerationMethods`.
    #[serde(default)]
    pub supported_generation_methods: Vec<String>,
}

impl From<WireModel> for ModelCard {
    /// The ListModels response carries no modality field. A blanket
    /// `Some(true)` here would outrank the live catalog and could not
    /// be corrected, so the verdict stays for the catalog.
    fn from(wire: WireModel) -> Self {
        Self {
            id: wire
                .name
                .strip_prefix("models/")
                .unwrap_or(&wire.name)
                .to_owned(),
            display_name: wire.display_name,
            description: wire.description,
            input_token_limit: wire.input_token_limit,
            output_token_limit: wire.output_token_limit,
            supports_vision: None,
            generation_methods: wire.supported_generation_methods,
            // ListModels carries no pricing; the provider step bakes the
            // curated table in.
            pricing: None,
            family: None,
            release_date: None,
        }
    }
}

/// Whether the model is a text chat model. `generateContent` alone does not
/// discriminate, video (veo), image (nano-banana), speech (tts, audio),
/// realtime (live), and specialty models expose it too, so non-chat
/// families are excluded by id. This is Gemini-catalog knowledge, which is
/// why it lives here and not on the shared [`ModelCard`].
pub fn is_text(model: &ModelCard) -> bool {
    const NON_TEXT: [&str; 13] = [
        "veo",
        "image",
        "nano-banana",
        "tts",
        "audio",
        "live",
        "omni",
        "transcribe",
        "embedding",
        "aqa",
        "computer-use",
        "robotics",
        "lyria",
    ];
    !NON_TEXT.iter().any(|marker| model.id.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::event::Event;
    use rhema_core::message::{Image, ImageMediaType};
    use rhema_core::request::{ChatParams, ToolSpec};
    use rhema_core::tool::ToolResult;
    use serde_json::json;

    #[test]
    fn thought_signatures_round_trip_through_provider_data() {
        // The wire part carries a signature; the mapped ToolCall must keep
        // it in the opaque provider_data slot.
        let part: WirePart = serde_json::from_str(
            // thoughtSignature lives on the Part, next to functionCall.
            r#"{"functionCall": {"id": "abc", "name": "list_dir", "args": {}},
                "thoughtSignature": "sig1"}"#,
        )
        .unwrap();
        let mut normalizer = PartNormalizer::new();
        let Some(NormalizedPart::ToolCall(call)) = normalizer.normalize(&part) else {
            panic!("expected a tool call");
        };
        assert_eq!(call.provider_data, json!({ "thoughtSignature": "sig1" }));

        // Replaying the call through history must put the signature back on
        // the wire part, the API rejects the replay otherwise.
        let request = rhema_core::request::ModelRequest {
            system: None,
            messages: vec![
                rhema_core::Message {
                    annotations: Default::default(),
                    role: rhema_core::Role::Assistant,
                    content: vec![rhema_core::Content::ToolCall(call)],
                },
                rhema_core::Message {
                    annotations: Default::default(),
                    role: rhema_core::Role::Tool,
                    content: vec![rhema_core::Content::ToolResult(ToolResult::success(
                        "abc", "ok",
                    ))],
                },
            ],
            tools: vec![],
            params: ChatParams::new("m"),
        };
        let body = serde_json::to_value(request_body(&request).unwrap()).unwrap();
        assert_eq!(body["contents"][0]["parts"][0]["thoughtSignature"], "sig1");
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "list_dir"
        );
    }

    #[test]
    fn assistant_thought_replays_from_provider_data() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_0".into(),
                    name: "a".into(),
                    args: json!({}),
                    provider_data: json!({
                        "thought": "I should check",
                        "thoughtSignature": "sig"
                    }),
                })],
            }],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        let parts = json["contents"][0]["parts"].as_array().expect("parts");

        // Thought part first, then the call, matching what the model
        // generated, and the signature still on the call part.
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0],
            json!({ "text": "I should check", "thought": true })
        );
        assert_eq!(parts[1]["functionCall"]["name"], "a");
        assert_eq!(parts[1]["thoughtSignature"], "sig");
    }

    #[test]
    fn a_call_without_a_signature_maps_to_null_provider_data() {
        let part: WirePart =
            serde_json::from_str(r#"{"functionCall": {"name": "list_dir", "args": {}}}"#).unwrap();
        let mut normalizer = PartNormalizer::new();
        let Some(NormalizedPart::ToolCall(call)) = normalizer.normalize(&part) else {
            panic!("expected a tool call");
        };
        assert_eq!(call.provider_data, serde_json::Value::Null);
    }

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

    fn chunk(json: &str) -> GenerateContentResponse {
        serde_json::from_str(json).expect("chunk fixture parses")
    }

    #[test]
    fn request_body_maps_roles_tools_and_params() {
        let req = ModelRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message::text(Role::User, "list files"),
                Message {
                    annotations: Default::default(),
                    role: Role::Assistant,
                    content: vec![
                        Content::Text("checking".into()),
                        Content::ToolCall(ToolCall {
                            id: "call_0".into(),
                            name: "list_dir".into(),
                            args: json!({ "path": "." }),

                            provider_data: serde_json::Value::Null,
                        }),
                    ],
                },
                Message {
                    annotations: Default::default(),
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
                model: "gemini-2.5-flash".into(),
                temperature: Some(0.2),
                top_p: Some(0.9),
                max_tokens: Some(256),
                stop: vec!["STOP".into()],
                session_id: None,
            },
        };

        let body = request_body(&req).expect("request maps");
        let json = serde_json::to_value(&body).expect("body serializes");

        assert_eq!(json["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(json["generationConfig"]["temperature"], json!(0.2_f32));
        assert_eq!(json["generationConfig"]["topP"], json!(0.9_f32));
        assert_eq!(json["generationConfig"]["maxOutputTokens"], json!(256));
        assert_eq!(json["generationConfig"]["stopSequences"], json!(["STOP"]));
        assert_eq!(
            json["tools"][0]["functionDeclarations"][0]["name"],
            "list_dir"
        );
        assert_eq!(
            json["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
            "object"
        );

        // Tool and following user turns merge into one user content.
        let contents = json["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "list files");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "checking");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "list_dir");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["args"]["path"], ".");
        assert!(contents[1]["parts"][1]["functionCall"].get("id").is_none());
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"],
            json!({ "name": "list_dir", "response": { "output": "[\"src\"]" } })
        );
        assert_eq!(contents[2]["parts"][1]["text"], "thanks");
    }

    #[test]
    fn system_messages_fold_into_the_system_instruction() {
        let req = ModelRequest {
            system: Some("a".into()),
            messages: vec![
                Message::text(Role::System, "b"),
                Message::text(Role::User, "hi"),
            ],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        }
        .normalized();

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(json["systemInstruction"]["parts"][0]["text"], "a\n\nb");
        assert_eq!(json["contents"].as_array().expect("contents").len(), 1);
        assert!(json.get("tools").is_none());
        assert!(json.get("generationConfig").is_none());
    }

    #[test]
    fn tool_results_with_unknown_ids_fall_back_to_the_id_as_name() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::failure("call_9", "boom"))],
            }],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["contents"][0]["parts"][0]["functionResponse"],
            json!({ "name": "call_9", "response": { "error": "boom" } })
        );
    }

    #[test]
    fn non_object_call_args_are_wrapped_and_missing_args_cleared() {
        let req = ModelRequest {
            system: None,
            messages: vec![
                Message {
                    annotations: Default::default(),
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: "call_0".into(),
                        name: "odd".into(),
                        args: json!("not an object"),

                        provider_data: serde_json::Value::Null,
                    })],
                },
                Message {
                    annotations: Default::default(),
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "bare".into(),
                        args: Value::Null,

                        provider_data: serde_json::Value::Null,
                    })],
                },
            ],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        let contents = json["contents"].as_array().expect("contents");
        assert_eq!(
            contents[0]["parts"][0]["functionCall"]["args"],
            json!({ "_rhema_args": "not an object" })
        );
        assert_eq!(contents[0]["parts"][1]["functionCall"]["args"], json!({}));
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
            params: ChatParams::new("gemini-2.5-flash"),
        };
        assert!(matches!(
            request_body(&no_messages),
            Err(ProviderError::InvalidRequest(_))
        ));
    }

    #[test]
    fn an_image_block_maps_to_an_inline_data_part() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message::text_with_images(
                Role::User,
                "what is this?",
                vec![Image::new(ImageMediaType::Png, "aGVsbG8=")],
            )],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["contents"][0]["parts"],
            json!([
                { "text": "what is this?" },
                { "inlineData": { "mimeType": "image/png", "data": "aGVsbG8=" } }
            ])
        );
    }

    #[test]
    fn image_parts_keep_the_message_block_order() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::User,
                content: vec![
                    Content::Text("first".into()),
                    Content::Image(Image::new(ImageMediaType::Webp, "d2VicA==")),
                    Content::Text("second".into()),
                ],
            }],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["contents"][0]["parts"],
            json!([
                { "text": "first" },
                { "inlineData": { "mimeType": "image/webp", "data": "d2VicA==" } },
                { "text": "second" }
            ])
        );
    }

    #[test]
    fn a_text_only_turn_serializes_bare_text_parts() {
        let req = ModelRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message::text(Role::User, "list files"),
                Message {
                    annotations: Default::default(),
                    role: Role::Tool,
                    content: vec![Content::ToolResult(ToolResult::success("call_0", "[]"))],
                },
            ],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-flash"),
        };

        // Byte-identical to the pre-image shape: text and functionResponse
        // parts only, no inlineData machinery anywhere.
        let json = serde_json::to_value(request_body(&req).expect("request maps"))
            .expect("body serializes");
        assert_eq!(
            json["systemInstruction"]["parts"],
            json!([{ "text": "be terse" }])
        );
        // The tool result merges into the same `user` content: consecutive
        // turns of one wire role collapse.
        assert_eq!(json["contents"].as_array().unwrap().len(), 1);
        assert_eq!(
            json["contents"][0]["parts"][0],
            json!({ "text": "list files" })
        );
        assert_eq!(
            json["contents"][0]["parts"][1],
            json!({ "functionResponse": { "name": "call_0", "response": { "output": "[]" } } })
        );
        assert!(
            !serde_json::to_string(&json).unwrap().contains("inlineData"),
            "{json}"
        );
    }

    #[test]
    fn text_chunks_assemble_end_turn_with_usage() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"text\":\"Hel\"},{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}],",
                    "\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2}}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        assert_eq!(
            out.message,
            Message {
                annotations: Default::default(),
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
                cached_input_tokens: 0,
            }
        );
        // The usage frame rides the chunk: the live tally precedes the
        // text it accounts for.
        assert_eq!(
            sink.0,
            vec![
                "Usage(Usage { input_tokens: 10, output_tokens: 2, cached_input_tokens: 0 })",
                "TextDelta(\"Hel\")",
                "TextDelta(\"lo\")",
            ]
        );
    }

    #[test]
    fn a_tool_results_images_ride_the_response_turn_as_inline_parts() {
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![rhema_core::Content::ToolResult(
                    ToolResult::success("call_0", "out")
                        .with_images(vec![Image::new(ImageMediaType::Png, "cG5n")]),
                )],
            }],
            tools: vec![],
            params: ChatParams::new("gemini-2.5-pro"),
        };

        let body = serde_json::to_value(request_body(&req).unwrap()).expect("body serializes");
        let parts = body["contents"][0]["parts"]
            .as_array()
            .expect("parts array");
        // The function response keeps its bare text output, no note.
        let text = parts[0]["functionResponse"]["response"]["output"]
            .as_str()
            .expect("output is text");
        assert_eq!(text, "out");
        // The image follows in the same turn as an inline part.
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(
            parts[1],
            serde_json::json!({ "inlineData": { "mimeType": "image/png", "data": "cG5n" } })
        );
    }

    #[test]
    fn function_calls_assemble_tool_use_with_synthesized_ids() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"functionCall\":{\"name\":\"echo\",\"args\":{\"x\":1}}}]},",
                    "\"finishReason\":\"STOP\"}],",
                    "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1}}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "echo".into(),
                args: json!({ "x": 1 }),

                provider_data: serde_json::Value::Null
            })]
        );
        assert_eq!(
            sink.0,
            vec![
                "Usage(Usage { input_tokens: 5, output_tokens: 1, cached_input_tokens: 0 })"
                    .to_owned(),
                "ToolCallStart { id: \"call_0\", name: \"echo\" }".to_owned(),
                "ToolCallArgsDelta { id: \"call_0\", delta: \"{\\\"x\\\":1}\" }".to_owned(),
            ]
        );
    }

    #[test]
    fn wire_call_ids_win_and_call_indices_stay_positional() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
                    "{\"functionCall\":{\"id\":\"abc\",\"name\":\"a\"}},",
                    "{\"functionCall\":{\"name\":\"b\"}}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
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
        assert_eq!(ids, vec!["abc".to_owned(), "call_1".to_owned()]);
    }

    #[test]
    fn missing_call_args_default_to_an_empty_object() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"functionCall\":{\"name\":\"bare\"}}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "bare".into(),
                args: json!({}),

                provider_data: serde_json::Value::Null
            })]
        );
    }

    #[test]
    fn a_named_thinking_effort_maps_to_a_thinking_budget() {
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: "gemini-2.5-flash".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::High,
                top_p: None,
                stop: Vec::new(),
                session_id: None,
            },
        };
        let body =
            serde_json::to_value(request_body(&req).expect("body builds")).expect("serializes");
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["thinkingBudget"], 32_768);
        assert_eq!(thinking["includeThoughts"], true);

        // Default sends no thinking config at all.
        req.params.thinking = ThinkingEffort::Default;
        let body =
            serde_json::to_value(request_body(&req).expect("body builds")).expect("serializes");
        assert!(body.get("generationConfig").is_none(), "{body}");
    }

    #[test]
    fn a_gemini_3_model_takes_a_named_level_and_an_older_one_the_budget() {
        let make = |model: &str, thinking| ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: model.into(),
                temperature: None,
                max_tokens: None,
                thinking,
                top_p: None,
                stop: Vec::new(),
                session_id: None,
            },
        };

        // Gemini 3 and later take the named level; 2.5 and earlier the
        // budget.
        for model in [
            "gemini-3.8-flash",
            "gemini-3.1-pro-preview",
            "gemini-4-carbon",
            "models/gemini-3-flash-preview",
        ] {
            assert_eq!(
                ThinkingStyle::for_model(model),
                ThinkingStyle::Level,
                "{model}"
            );
        }
        for model in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "models/gemini-2.5-flash",
        ] {
            assert_eq!(
                ThinkingStyle::for_model(model),
                ThinkingStyle::Budget,
                "{model}"
            );
        }

        let body = serde_json::to_value(
            request_body(&make("gemini-3.8-flash", ThinkingEffort::High)).expect("body builds"),
        )
        .expect("serializes");
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["thinkingLevel"], "HIGH");
        assert_eq!(thinking["includeThoughts"], true);
        assert!(thinking.get("thinkingBudget").is_none(), "{body}");

        // The one documented level family without MEDIUM takes HIGH.
        let body = serde_json::to_value(
            request_body(&make("gemini-3-pro-preview", ThinkingEffort::Medium))
                .expect("body builds"),
        )
        .expect("serializes");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        // The 3.1 refresh added the level.
        let body = serde_json::to_value(
            request_body(&make("gemini-3.1-pro-preview", ThinkingEffort::Medium))
                .expect("body builds"),
        )
        .expect("serializes");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );

        // An older model keeps the budget and sends no level.
        let body = serde_json::to_value(
            request_body(&make("gemini-2.5-pro", ThinkingEffort::High)).expect("body builds"),
        )
        .expect("serializes");
        let thinking = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(thinking["thinkingBudget"], 32_768);
        assert_eq!(thinking["includeThoughts"], true);
        assert!(thinking.get("thinkingLevel").is_none(), "{body}");

        // Default sends no thinking config at all.
        let body = serde_json::to_value(
            request_body(&make("gemini-3.8-flash", ThinkingEffort::Default)).expect("body builds"),
        )
        .expect("serializes");
        assert!(body.get("generationConfig").is_none(), "{body}");
    }

    #[test]
    fn thought_parts_stream_but_never_reach_the_message() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
                    "{\"text\":\"thinking\",\"thought\":true},",
                    "{\"text\":\"\"},{\"text\":\"answer\"}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        // The thought text streams live; the message carries only the
        // reply, and the empty part drops.
        assert_eq!(out.message.content, vec![Content::Text("answer".into())]);
        assert_eq!(
            sink.0,
            vec![
                "ThinkingDelta(\"thinking\")".to_owned(),
                "TextDelta(\"answer\")".to_owned()
            ]
        );
    }

    #[test]
    fn thought_before_a_function_call_is_kept_for_replay() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
                    "{\"text\":\"I should check\",\"thought\":true},",
                    "{\"functionCall\":{\"name\":\"a\",\"args\":{}},",
                    "\"thoughtSignature\":\"sig1\"}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        // The thought rides in the call's provider data next to the
        // signature; it never joins the message as plain text.
        assert_eq!(
            out.message.content,
            vec![Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "a".into(),
                args: json!({}),
                provider_data: json!({
                    "thought": "I should check",
                    "thoughtSignature": "sig1"
                }),
            })]
        );
        assert_eq!(
            sink.0,
            vec![
                "ThinkingDelta(\"I should check\")".to_owned(),
                "ToolCallStart { id: \"call_0\", name: \"a\" }".to_owned(),
                "ToolCallArgsDelta { id: \"call_0\", delta: \"{}\" }".to_owned(),
            ]
        );
    }

    #[test]
    fn finish_reasons_map_onto_stop_reasons() {
        let cases: &[(&str, StopReason)] = &[
            ("STOP", StopReason::EndTurn),
            ("FINISH_REASON_UNSPECIFIED", StopReason::EndTurn),
            ("MAX_TOKENS", StopReason::MaxTokens),
            ("SAFETY", StopReason::Other("SAFETY".into())),
            ("RECITATION", StopReason::Other("RECITATION".into())),
            (
                "MALFORMED_FUNCTION_CALL",
                StopReason::Other("MALFORMED_FUNCTION_CALL".into()),
            ),
        ];
        for (reason, expected) in cases {
            let mut assembler = ChunkAssembler::new();
            let mut sink = Collector::new();
            assembler
                .push(
                    chunk(&format!(
                        "{{\"candidates\":[{{\"finishReason\":\"{reason}\"}}]}}"
                    )),
                    &mut sink,
                )
                .expect("chunk is not an error");
            assert_eq!(assembler.finish().stop_reason, *expected, "for {reason}");
        }

        // No finish reason at all defaults to EndTurn.
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
        .push(
            chunk("{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"x\"}]}}]}"),
            &mut sink,
        )
        .expect("chunk is not an error");
        assert_eq!(assembler.finish().stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn a_blocked_prompt_yields_an_empty_message_and_other() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk("{\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}"),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        assert_eq!(out.message.content, Vec::<Content>::new());
        assert_eq!(
            out.stop_reason,
            StopReason::Other("prompt blocked by SAFETY".into())
        );
    }

    #[test]
    fn last_finish_reason_and_usage_win() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"text\":\"x\"}]}}],",
                    "\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":0}}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"finishReason\":\"MAX_TOKENS\"}],",
                    "\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":7,",
                    "\"cachedContentTokenCount\":3}}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        let out = assembler.finish();

        assert_eq!(out.stop_reason, StopReason::MaxTokens);
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
    fn thinking_tokens_fold_into_output_tokens() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],",
                    "\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":2,",
                    "\"thoughtsTokenCount\":8}}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        // Gemini bills thoughts as output; the sum is the billed figure.
        assert_eq!(
            assembler.finish().usage,
            Usage {
                input_tokens: 10,
                output_tokens: 10,
                cached_input_tokens: 0,
            }
        );
    }

    #[test]
    fn a_finish_reason_or_block_is_the_terminal_proof() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        assert!(!assembler.saw_terminal());
        assembler
            .push(
                chunk("{\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}"),
                &mut sink,
            )
            .expect("chunk is not an error");
        assert!(assembler.saw_terminal());

        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        // A text chunk with no finishReason is stream-middle, not the
        // end: the drain behind the assembler refuses here.
        assembler
            .push(
                chunk(concat!(
                    "{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":",
                    "[{\"text\":\"half\"}]}}]}"
                )),
                &mut sink,
            )
            .expect("chunk is not an error");
        assert!(!assembler.saw_terminal());
        assembler
            .push(
                chunk("{\"candidates\":[{\"finishReason\":\"STOP\"}]}"),
                &mut sink,
            )
            .expect("chunk is not an error");
        assert!(assembler.saw_terminal());
    }

    #[test]
    fn a_mid_stream_error_chunk_fails_the_push() {
        let mut assembler = ChunkAssembler::new();
        let mut sink = Collector::new();
        let err = assembler
            .push(
                chunk(concat!(
                    "{\"error\":{\"code\":500,\"message\":\"inference failed\",",
                    "\"status\":\"UNAVAILABLE\"}}"
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
            "{\"error\":{\"code\":400,\"message\":\"bad request\",\"status\":\"INVALID_ARGUMENT\"}}",
        )
        .expect("error body parses");
        assert_eq!(body.message("raw"), "bad request");

        let unshaped: WireErrorBody = serde_json::from_str("not even json").unwrap_or_default();
        assert_eq!(unshaped.message("not even json"), "not even json");
    }

    #[test]
    fn a_retry_info_detail_names_its_reset() {
        let body: WireErrorBody = serde_json::from_str(
            r#"{
                "error": {
                    "code": 429,
                    "message": "Resource has been exhausted",
                    "status": "RESOURCE_EXHAUSTED",
                    "details": [
                        {"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "FREE_QUOTA"},
                        {"@type": "type.googleapis.com/google.rpc.RetryInfo",
                         "retryDelay": "5.5s"}
                    ]
                }
            }"#,
        )
        .expect("error body parses");
        assert_eq!(body.retry_delay(), Some("5.5s"));

        let bare: WireErrorBody = serde_json::from_str(
            r#"{"error":{"code":429,"message":"no","status":"RESOURCE_EXHAUSTED"}}"#,
        )
        .expect("error body parses");
        assert_eq!(bare.retry_delay(), None);
    }

    #[test]
    fn wire_model_normalizes_and_chat_capability_is_text_discriminates() {
        let wire: WireModel = serde_json::from_str(
            r#"{
                "name": "models/gemini-3.8-flash",
                "displayName": "Gemini 3.8 Flash",
                "description": "the current flash",
                "inputTokenLimit": 1048576,
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            }"#,
        )
        .unwrap();
        let model: ModelCard = wire.into();
        assert_eq!(model.id, "gemini-3.8-flash");
        assert_eq!(model.display_name, "Gemini 3.8 Flash");
        assert_eq!(model.input_token_limit, Some(1_048_576));
        assert_eq!(model.output_token_limit, None);
        assert!(model.supports_chat());
        assert!(is_text(&model));
        // No modality on the wire: the live catalog answers vision.
        assert_eq!(model.supports_vision, None);

        let tts: ModelCard = serde_json::from_str::<WireModel>(
            r#"{
                "name": "models/gemini-3.8-flash-tts",
                "supportedGenerationMethods": ["generateContent", "generateAudio"]
            }"#,
        )
        .unwrap()
        .into();
        assert_eq!(tts.id, "gemini-3.8-flash-tts");
        assert!(
            tts.supports_chat(),
            "tts models still expose generateContent"
        );
        assert!(
            !is_text(&tts),
            "the id marker, not the method list, excludes speech models"
        );
    }
}
