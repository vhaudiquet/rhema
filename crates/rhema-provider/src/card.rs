//! The catalog row every adapter's listing normalizes into.

use serde::{Deserialize, Serialize};

/// One model in a provider's catalog, normalized for consumers: the id
/// the adapter's chat API accepts, the listing's display fields, and the
/// adapter's price for it when known. Adapters decide which of their
/// listed models are text chat models and `list_models` only hands out
/// those — the marker data is per provider, so the verdict never rides on
/// this row. Not `Eq`: the price is floating point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCard {
    /// Model id usable as the request's `model` value (e.g.
    /// `gemini-3.8-flash`).
    pub id: String,
    /// Human-readable name (e.g. `Gemini 3.8 Flash`).
    pub display_name: String,
    /// What the model is for, from the listing.
    pub description: String,
    /// Prompt ceiling in tokens, when advertised.
    pub input_token_limit: Option<u64>,
    /// Output ceiling in tokens, when advertised.
    pub output_token_limit: Option<u64>,
    /// What the listing says the model can do; chat-capable rows carry
    /// `generateContent`, the adapters' shared chat marker.
    pub generation_methods: Vec<String>,
    /// The adapter's per-MTok price for the model, when known: the
    /// listing's own price when it carries one, else the adapter's
    /// curated knowledge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<crate::pricing::Pricing>,
}

impl ModelCard {
    /// Whether the model can be used for chat: the listing's methods
    /// include the chat marker.
    pub fn supports_chat(&self) -> bool {
        self.generation_methods
            .iter()
            .any(|m| m == "generateContent")
    }
}
