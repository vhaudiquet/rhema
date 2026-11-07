//! The catalog row every adapter's listing normalizes into, and the
//! seam the listings share.

use serde::{Deserialize, Serialize};

use async_trait::async_trait;
use rhema_core::ProviderError;

/// The live-catalog seam: a provider's models, as they list today.
/// Every adapter in the family implements it, so a consumer holding
/// nothing but a provider name can still fetch the catalog — no
/// concrete type required. The listing stays off the turn seam's
/// retry: a catalog fetch either answers or fails, no ladder.
#[async_trait]
pub trait ModelListing: Send + Sync {
    /// The provider's text-chat models, normalized to [`ModelCard`]
    /// rows — pricing included when the provider publishes it.
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError>;
}

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
    /// Whether the model accepts image input, when the listing or the
    /// adapter's own knowledge says so. Tri-state: `None` — the common
    /// case — is unknown, not "no": most listings carry no modality
    /// data. A `Some` verdict rides the catalog row; sending an image
    /// to a `false` model fails server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> ModelCard {
        ModelCard {
            id: "gemini-3.8-flash".to_owned(),
            display_name: "Gemini 3.8 Flash".to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            supports_vision: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }

    #[test]
    fn the_vision_flag_serializes_only_when_known_and_loads_as_unknown() {
        let unknown = card();
        let json = serde_json::to_string(&unknown).unwrap();
        // `None` is unknown, not "no", so it never reaches the payload.
        assert!(!json.contains("supports_vision"), "{json}");
        assert_eq!(serde_json::from_str::<ModelCard>(&json).unwrap(), unknown);

        let known = ModelCard {
            supports_vision: Some(true),
            ..card()
        };
        let json = serde_json::to_string(&known).unwrap();
        assert!(json.contains("\"supports_vision\":true"), "{json}");
        assert_eq!(serde_json::from_str::<ModelCard>(&json).unwrap(), known);

        // A payload from before the field existed loads as unknown.
        let legacy: ModelCard = serde_json::from_str(
            r#"{"id":"m","display_name":"m","description":"","generation_methods":[]}"#,
        )
        .unwrap();
        assert_eq!(legacy.supports_vision, None);
    }
}
