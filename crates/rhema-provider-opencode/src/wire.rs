//! The Console model catalog and its mapping onto the shared
//! [`ModelCard`] row.
//!
//! `GET {base}/models` returns more than the generic OpenAI listing:
//! every entry publishes its purpose (`type`), modalities, context and
//! output ceilings, and the per-MTok prices Console charges. This module
//! reads that shape and drops everything but the rows a coding agent can
//! drive through chat completions.
//!
//! Prices arrive already normalized per million tokens (the same unit
//! [`Pricing`] spells), so no conversion happens here. Free rows carry
//! zeros; the provider filters them for plans that cannot serve them.

use rhema_provider::{ModelCard, Pricing};
use serde::Deserialize;

/// One page of `GET {base}/models`.
#[derive(Debug, Default, Deserialize)]
pub struct WireModelsPage {
    /// The catalog rows.
    #[serde(default)]
    pub data: Vec<WireModel>,
}

/// One catalog row. Dozens of fields exist; only the ones a picker row
/// shows are kept.
#[derive(Debug, Default, Deserialize)]
pub struct WireModel {
    /// Model id usable as the request's `model` value.
    #[serde(default)]
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// What the model is for.
    #[serde(default)]
    pub description: String,
    /// The row's purpose: `chat` for the text models this adapter serves.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Input modalities, when advertised.
    #[serde(default)]
    pub modalities: Option<WireModalities>,
    /// Context and output ceilings, when advertised.
    #[serde(default)]
    pub limit: Option<WireLimit>,
    /// Prices, when advertised.
    #[serde(default)]
    pub cost: Option<WireCost>,
}

/// The `modalities` object: what the model accepts and emits.
#[derive(Debug, Default, Deserialize)]
pub struct WireModalities {
    /// Input modality names (`text`, `image/png`, ...).
    #[serde(default)]
    pub input: Vec<String>,
}

/// The `limit` object: context and output ceilings in tokens.
#[derive(Debug, Default, Deserialize)]
pub struct WireLimit {
    /// The whole context window.
    #[serde(default)]
    pub context: Option<u64>,
    /// The input ceiling, when tighter than the context window.
    #[serde(default)]
    pub input: Option<u64>,
    /// The output ceiling.
    #[serde(default)]
    pub output: Option<u64>,
}

/// The `cost` object. Only text prices matter to a chat adapter.
#[derive(Debug, Default, Deserialize)]
pub struct WireCost {
    /// Text-token prices.
    #[serde(default)]
    pub text: Option<WireTextCost>,
}

/// Text input and output prices.
#[derive(Debug, Default, Deserialize)]
pub struct WireTextCost {
    /// The input price.
    #[serde(default)]
    pub input: Option<WirePrice>,
    /// The output price.
    #[serde(default)]
    pub output: Option<WirePrice>,
}

/// One direction's price, USD per million tokens.
#[derive(Debug, Default, Deserialize)]
pub struct WirePrice {
    /// The base rate.
    #[serde(default)]
    pub price: Option<f64>,
    /// The cached-input rates, on the input direction.
    #[serde(default)]
    pub cache: Option<WireCache>,
}

/// The `cache` object: what a cached read costs.
#[derive(Debug, Default, Deserialize)]
pub struct WireCache {
    /// The cached-read rate.
    #[serde(default)]
    pub read: Option<f64>,
}

/// Whether the row is a text chat model this adapter can serve.
pub fn is_text(model: &WireModel) -> bool {
    model.kind == "chat"
}

/// The row's vision verdict: an advertised input modality names an
/// image type or not — a real `Some` either way. No modality list stays
/// unknown rather than guessed.
fn vision_of(model: &WireModel) -> Option<bool> {
    let inputs = model.modalities.as_ref()?.input.as_slice();
    (!inputs.is_empty()).then(|| {
        inputs
            .iter()
            .any(|modality| modality == "image" || modality.starts_with("image/"))
    })
}

/// The row's per-MTok price, when both directions publish one.
fn pricing_of(model: &WireModel) -> Option<Pricing> {
    let text = model.cost.as_ref()?.text.as_ref()?;
    let input = text.input.as_ref()?;
    let output = text.output.as_ref()?;
    let input_per_mtok = input.price?;
    let output_per_mtok = output.price?;
    if !input_per_mtok.is_finite() || !output_per_mtok.is_finite() {
        return None;
    }
    let cached_input_per_mtok = input
        .cache
        .as_ref()
        .and_then(|cache| cache.read)
        .filter(|rate| rate.is_finite());
    Some(Pricing {
        input_per_mtok,
        output_per_mtok,
        cached_input_per_mtok,
    })
}

/// Whether a row is entirely free. Console's free-tier chat models are
/// served only inside OpenCode itself; a harness picks paid rows, or
/// Go's own included free ones.
pub fn is_free(model: &ModelCard) -> bool {
    model
        .pricing
        .is_some_and(|pricing| pricing.input_per_mtok == 0.0 && pricing.output_per_mtok == 0.0)
}

impl From<WireModel> for ModelCard {
    fn from(model: WireModel) -> Self {
        let supports_vision = vision_of(&model);
        let pricing = pricing_of(&model);
        let limit = model.limit.unwrap_or_default();
        Self {
            id: model.id,
            display_name: model.name,
            description: model.description,
            input_token_limit: limit.input.or(limit.context),
            output_token_limit: limit.output,
            supports_vision,
            generation_methods: vec!["generateContent".to_owned()],
            pricing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paid_row_carries_its_window_prices_and_modalities() {
        let page: WireModelsPage = serde_json::from_value(serde_json::json!({
            "data": [{
                "id": "deepseek-v4-flash",
                "name": "DeepSeek V4.1 Flash",
                "description": "Reasoning and agentic coding",
                "type": "chat",
                "modalities": { "input": ["text", "image/png"] },
                "limit": { "context": 1_000_000, "output": 384_000 },
                "cost": { "text": {
                    "input": { "price": 0.3, "cache": { "read": 0.006 } },
                    "output": { "price": 1.2 }
                }}
            }]
        }))
        .unwrap();
        let model = &page.data[0];
        assert!(is_text(model));
        let card = ModelCard::from(page.data.into_iter().next().unwrap());
        assert_eq!(card.input_token_limit, Some(1_000_000));
        assert_eq!(card.output_token_limit, Some(384_000));
        assert_eq!(card.supports_vision, Some(true));
        let pricing = card.pricing.expect("priced");
        assert_eq!(pricing.input_per_mtok, 0.3);
        assert_eq!(pricing.output_per_mtok, 1.2);
        assert_eq!(pricing.cached_input_per_mtok, Some(0.006));
        assert!(!is_free(&card));
    }

    #[test]
    fn a_free_row_is_recognizable_and_an_unpriced_one_is_not_a_false_free() {
        let free: WireModel = serde_json::from_value(serde_json::json!({
            "id": "free",
            "type": "chat",
            "cost": { "text": {
                "input": { "price": 0.0 },
                "output": { "price": 0.0 }
            }}
        }))
        .unwrap();
        assert!(is_free(&ModelCard::from(free)));

        let unpriced: WireModel = serde_json::from_value(serde_json::json!({
            "id": "local",
            "type": "chat"
        }))
        .unwrap();
        assert!(!is_free(&ModelCard::from(unpriced)));
    }

    #[test]
    fn only_chat_rows_and_advertised_modality_verdicts_survive() {
        let page: WireModelsPage = serde_json::from_value(serde_json::json!({
            "data": [
                { "id": "embed", "type": "embedding" },
                { "id": "text", "type": "chat", "modalities": { "input": ["text"] } },
                { "id": "quiet", "type": "chat" }
            ]
        }))
        .unwrap();
        let text: Vec<ModelCard> = page
            .data
            .into_iter()
            .filter(is_text)
            .map(ModelCard::from)
            .collect();
        assert_eq!(text.len(), 2);
        assert_eq!(text[0].supports_vision, Some(false));
        assert_eq!(text[1].supports_vision, None, "no modalities stays unknown");
    }
}
