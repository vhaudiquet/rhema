//! The Mistral and Vibe catalog wire types and their normalization into
//! the shared catalog row.
//!
//! The public API serves a bare JSON array, not an OpenAI-shaped `data`
//! envelope; Vibe's catalog wraps the same cards in `{"models": [...]}`.
//! Each entry carries its capabilities, context window, and lifecycle
//! flags; the adapter keeps the chat-capable models and folds everything
//! else into [`ModelCard`].

use std::collections::HashMap;

use serde::Deserialize;

use rhema_provider::{Allowance, ModelCard};

/// One entry of the `/v1/models` array.
#[derive(Debug, Clone, Deserialize)]
pub struct WireModel {
    /// The id the chat API accepts.
    pub id: String,
    /// Human-readable name, when the listing carries one.
    #[serde(default)]
    pub name: Option<String>,
    /// The Vibe catalog's display label, when the listing carries one.
    #[serde(default)]
    pub label: Option<String>,
    /// What the model is for, when the listing carries it.
    #[serde(default)]
    pub description: Option<String>,
    /// What the model can do. Absent or null entries read as unknown.
    #[serde(default)]
    pub capabilities: Option<WireCapabilities>,
    /// Prompt ceiling in tokens.
    #[serde(default)]
    pub max_context_length: Option<u64>,
    /// Retired models stay listed; the picker does not offer them.
    #[serde(default)]
    pub archived: Option<bool>,
    /// Models Mistral does not offer to ordinary accounts.
    #[serde(default)]
    pub internal: Option<bool>,
    /// The other names a request can use for the model.
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
    /// Tuning metadata the catalog carries, when present. Only the
    /// Vibe's `model-catalog` route publishes these.
    #[serde(default)]
    pub parameters: Option<WireParameters>,
}

/// A catalog entry's tuning block. `reasoning` is the exhaustive list
/// of `reasoning_effort` values the model accepts; an absent or empty
/// one reads as unknown, never as none.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireParameters {
    /// The model's accepted `reasoning_effort` spellings.
    #[serde(default)]
    pub reasoning: Option<Vec<String>>,
}

/// The capability flags a model card carries. Absent flags are unknown,
/// not false: the listing normally carries the whole object.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireCapabilities {
    /// Whether the model answers chat completions.
    #[serde(default)]
    pub completion_chat: Option<bool>,
    /// Whether the model accepts images.
    #[serde(default)]
    pub vision: Option<bool>,
}

/// The `/v1/models` response. Mistral serves a bare array; some
/// compatible deployments wrap it in an OpenAI-shaped `{"data": [...]}`
/// or a `{"models": [...]}` envelope. All three are read.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WireModelsPage {
    /// The API's own shape: the cards at the top level.
    Bare(Vec<WireModel>),
    /// A listing that follows the OpenAI convention.
    Envelope {
        /// The cards.
        data: Vec<WireModel>,
    },
    /// A listing under a `models` key.
    Named {
        /// The cards.
        models: Vec<WireModel>,
    },
}

impl WireModelsPage {
    /// The cards, whichever envelope carried them.
    pub fn into_models(self) -> Vec<WireModel> {
        match self {
            Self::Bare(models) | Self::Envelope { data: models } | Self::Named { models } => models,
        }
    }
}

/// Normalizes the page into the rows the picker can use: chat-capable,
/// not archived, not internal. Unknown entries still become rows when
/// the listing omits capabilities, since Mistral only lists usable
/// models; the id-based fallback keeps no model from disappearing.
pub fn normalize_models(page: WireModelsPage) -> Vec<ModelCard> {
    page.into_models()
        .into_iter()
        .filter(|model| model.archived != Some(true) && model.internal != Some(true))
        .filter(|model| {
            model
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.completion_chat)
                .unwrap_or(true)
        })
        .map(|model| {
            let id = model.id;
            let capabilities = model.capabilities.unwrap_or_default();
            ModelCard {
                display_name: model
                    .name
                    .or(model.label)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| id.clone()),
                description: model.description.unwrap_or_default(),
                input_token_limit: model.max_context_length,
                output_token_limit: None,
                supports_vision: capabilities.vision,
                generation_methods: vec!["generateContent".to_owned()],
                pricing: crate::pricing::model_pricing(&id),
                id,
                family: None,
                release_date: None,
            }
        })
        .collect()
}

/// The reasoning-effort levels each model accepts, keyed by every name
/// a request can use, the id and each alias. Only the entries whose
/// catalog `parameters.reasoning` is present and non-empty appear: a
/// model the catalog does not describe stays out, and its requests fly
/// as written rather than clamped to a guess.
pub fn reasoning_levels(page: WireModelsPage) -> HashMap<String, Vec<String>> {
    let mut levels: HashMap<String, Vec<String>> = HashMap::new();
    for model in page.into_models() {
        let Some(reasoning) = model
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.reasoning.as_ref())
            .filter(|reasoning| !reasoning.is_empty())
        else {
            continue;
        };
        levels
            .entry(model.id.clone())
            .or_default()
            .clone_from(reasoning);
        for alias in model.aliases.unwrap_or_default() {
            levels.entry(alias).or_default().clone_from(reasoning);
        }
    }
    levels
}

// --- Billing: the plan's Vibe Code budget -> an account allowance ---

/// The `GET /billing/budget` response: the plan's monthly Vibe Code
/// budget and its API counterpart. Consumer fields, the ones a plain
/// API key reads, filter down to [`WireBudgetDetail`].
#[derive(Debug, Clone, Deserialize)]
pub struct WireBudget {
    /// The Vibe plan's allowance. Absent when the key has no Vibe
    /// subscription or the platform does not publish budgets.
    #[serde(default)]
    pub vibe_budget: Option<WireBudgetDetail>,
}

/// One plan budget: how much of it the month has spent and what the
/// whole of it is.
#[derive(Debug, Clone, Deserialize)]
pub struct WireBudgetDetail {
    /// The share of the budget already spent, 0-100.
    pub usage_percentage: f64,
    /// The whole budget, in its own currency.
    pub initial_budget: f64,
    /// The ISO currency code the figures are in (`"EUR"`), when known.
    #[serde(default)]
    pub currency: Option<String>,
}

/// The budget's remaining spend, `"$12.34"`, in the budget's currency.
/// `None` when the response names no usable detail, a shape drift maps
/// to no badge, never to a wrong figure. The amount is money, unit
/// included, so the kind stays empty: the badge drawn beside it is the
/// money glyph already.
pub fn allowance(budget: &WireBudget) -> Option<Allowance> {
    let vibe = budget.vibe_budget.as_ref()?;
    // A percentage beyond 100 (pay-as-you-go can overrun the plan) still
    // reads as nothing left.
    let left = (vibe.initial_budget * ((100.0 - vibe.usage_percentage) / 100.0)).max(0.0);
    let amount = match vibe.currency.as_deref() {
        Some("EUR") => format!("\u{20ac}{left:.2}"),
        Some("USD") => format!("${left:.2}"),
        Some(other) => format!("{left:.2} {other}"),
        None => format!("{left:.2}"),
    };
    Some(Allowance {
        amount,
        kind: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_levels_key_ids_and_aliases_and_skip_undescribed_models() {
        let levels = reasoning_levels(
            serde_json::from_value(serde_json::json!([
                {
                    "id": "zai-glm-5-3",
                    "aliases": ["glm-5-3"],
                    "capabilities": {"completion_chat": true},
                    "parameters": {"reasoning": ["low", "high", "max"]}
                },
                {
                    "id": "mistral-large-4-0",
                    "capabilities": {"completion_chat": true}
                },
                {
                    "id": "blank-model",
                    "capabilities": {"completion_chat": true},
                    "parameters": {"reasoning": []}
                }
            ]))
            .expect("the listing parses"),
        );
        let glm = vec!["low".to_owned(), "high".to_owned(), "max".to_owned()];
        assert_eq!(levels.get("zai-glm-5-3"), Some(&glm));
        assert_eq!(levels.get("glm-5-3"), Some(&glm));
        assert!(!levels.contains_key("mistral-large-4-0"));
        assert!(!levels.contains_key("blank-model"));
    }

    #[test]
    fn the_budget_s_remaining_spend_carries_its_currency() {
        use rhema_provider::Allowance;

        let budget: WireBudget = serde_json::from_value(serde_json::json!({
            "vibe_budget": {
                "usage_percentage": 0.2276,
                "initial_budget": 255.0,
                "currency": "EUR",
                "reset_at": "2026-12-01T00:00:00Z"
            }
        }))
        .expect("the budget parses");
        assert_eq!(
            allowance(&budget),
            Some(Allowance {
                amount: "\u{20ac}254.42".to_owned(),
                kind: String::new()
            })
        );

        let over = serde_json::from_value(serde_json::json!({
            "vibe_budget": {
                "usage_percentage": 190.0,
                "initial_budget": 30.0,
                "currency": "USD"
            }
        }))
        .expect("the budget parses");
        assert_eq!(allowance(&over).map(|a| a.amount), Some("$0.00".to_owned()));

        let none: WireBudget =
            serde_json::from_value(serde_json::json!({})).expect("an empty budget parses");
        assert_eq!(allowance(&none), None);
    }

    fn page() -> WireModelsPage {
        serde_json::from_value(serde_json::json!([
            {
                "id": "mistral-large-4-0",
                "name": "Mistral Large 4",
                "description": "A general-purpose multimodal model.",
                "capabilities": {"completion_chat": true, "vision": true},
                "max_context_length": 1048576
            },
            {
                "id": "codestral-embed",
                "capabilities": {"completion_chat": false}
            },
            {
                "id": "old-model",
                "capabilities": {"completion_chat": true},
                "archived": true
            },
            {
                "id": "internal-model",
                "capabilities": {"completion_chat": true},
                "internal": true
            }
        ]))
        .expect("the listing parses")
    }

    #[test]
    fn only_servable_chat_models_survive() {
        let cards = normalize_models(page());
        assert_eq!(cards.len(), 1, "{cards:?}");
        let card = &cards[0];
        assert_eq!(card.id, "mistral-large-4-0");
        assert_eq!(card.display_name, "Mistral Large 4");
        assert_eq!(card.description, "A general-purpose multimodal model.");
        assert_eq!(card.input_token_limit, Some(1_048_576));
        assert_eq!(card.supports_vision, Some(true));
        assert!(card.supports_chat());
        assert_eq!(card.pricing.expect("priced").input_per_mtok, 0.70);
    }

    #[test]
    fn a_blank_name_falls_back_to_the_id_and_unknown_prices_stay_unset() {
        let cards = normalize_models(
            serde_json::from_value(serde_json::json!([
                {
                    "id": "custom-model",
                    "name": "  ",
                    "capabilities": {"completion_chat": true}
                }
            ]))
            .expect("parses"),
        );
        assert_eq!(cards[0].display_name, "custom-model");
        assert_eq!(cards[0].pricing, None);
        assert_eq!(cards[0].supports_vision, None);
    }

    #[test]
    fn null_flags_and_a_vibe_label_stay_readable() {
        let cards = normalize_models(
            serde_json::from_value(serde_json::json!([
                {
                    "id": "vibe-model",
                    "label": "Vibe Model",
                    "capabilities": null,
                    "archived": null,
                    "internal": null
                }
            ]))
            .expect("a null flag is not fatal"),
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].display_name, "Vibe Model");
        assert!(cards[0].supports_chat());
    }

    #[test]
    fn openai_and_named_envelopes_read_as_pages() {
        let wrapped: WireModelsPage =
            serde_json::from_value(serde_json::json!({"data": [{"id": "one"}]})).expect("data");
        let named: WireModelsPage =
            serde_json::from_value(serde_json::json!({"models": [{"id": "two"}]})).expect("models");
        assert_eq!(normalize_models(wrapped)[0].id, "one");
        assert_eq!(normalize_models(named)[0].id, "two");
    }

    #[test]
    fn an_entry_without_capabilities_stays_chat_capable() {
        let cards = normalize_models(
            serde_json::from_value(serde_json::json!([
                {"id": "bare-model"}
            ]))
            .expect("parses"),
        );
        assert_eq!(cards.len(), 1);
        assert!(cards[0].supports_chat());
    }
}
