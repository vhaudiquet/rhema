//! llama.cpp's dialect of the OpenAI chat-completions wire, plus the
//! models/props parsing that is llama.cpp's alone.
//!
//! The shared chat-completions vocabulary — request building, chunk
//! assembly, the SSE drain — lives in
//! [`rhema_provider_openai_compatible`]; this crate teaches it the few
//! things llama.cpp does differently and reuses the rest:
//!
//! - The thinking effort is a server-side knob (a launch flag, not a
//!   request field): no effort parameter is ever sent.
//! - An assistant turn never replays its chain of thought: llama.cpp
//!   re-derives reasoning per step, and rejects the reasoning fields.
//!   `ToolCall::provider_data` is ignored entirely — llama.cpp call ids
//!   are plain strings that round-trip on the wire, so there is nothing
//!   opaque to carry.
//! - A malformed streamed arguments payload wraps as
//!   `{"_llama_args_text": <raw>}` so the call still round-trips.
//!
//! Everything else — roles, content parts for images, tool-call
//! fragment assembly by index, the `[DONE]` sentinel, usage on the
//! final chunk — is the shared dialect's behavior, documented there.
//!
//! What remains here is the listing side: `/v1/models` shapes that
//! drifted across llama.cpp versions and modes, and `/props` context
//! lookup for single-model and router servers.

use rhema_provider::ModelCard;
use rhema_provider_openai_compatible::wire::Dialect;
use serde::Deserialize;

/// llama.cpp's dialect: the shared vocabulary, minus the fields the
/// server rejects, with its own malformed-arguments wrapper.
pub const LLAMA_CPP: Dialect = Dialect {
    args_key: "_llama_args_text",
    assistant_reasoning_key: None,
    effort_key: "reasoning_effort",
    sends_stream_options: true,
    reasoning_effort: rhema_provider_openai_compatible::wire::no_effort,
};

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
    /// `supports_vision` stays `None`: the server exposes no modality
    /// metadata — neither `/v1/models` nor `/props` carries any, and vision
    /// is a property of the loaded `mmproj` projector rather than of the
    /// model id — so unknown is the only honest verdict.
    fn from(model: LlamaModel) -> Self {
        Self {
            display_name: model.id.clone(),
            id: model.id,
            description: String::new(),
            input_token_limit: model.context_length,
            // The listing reports one shared context window and no
            // per-direction output ceiling.
            output_token_limit: None,
            supports_vision: None,
            // Every listed model is offered through chat completions.
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
