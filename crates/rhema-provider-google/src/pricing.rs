//! Pay-as-you-go prices for the chat models, USD per 1M tokens.
//!
//! The ListModels API carries no pricing, so this is a curated table.
//! Source: ai.google.dev/gemini-api/docs/pricing (fetched 2026-10-06);
//! pro entries are the ≤200K-token tier, and the 3.6–3.8 flash prices are
//! the promo rate valid through 2026-12-31. Prices drift — update here
//! when they do; unknown models simply show no cost.

use crate::wire::GeminiModel;
use serde::{Deserialize, Serialize};

/// Input and output price, USD per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    /// Price per 1M input tokens.
    pub input_per_mtok: f64,
    /// Price per 1M output tokens.
    pub output_per_mtok: f64,
    /// Price per 1M input tokens served from the provider's prompt
    /// cache; `None` when the provider publishes no cached rate.
    pub cached_input_per_mtok: Option<f64>,
}

impl Pricing {
    /// A price pair at Gemini's implicit-cache discount: cached input
    /// reads at a quarter of base input.
    fn with_cached_input(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
            cached_input_per_mtok: Some(input_per_mtok * 0.25),
        }
    }
}

/// The table entry for `id`, or `None` when unknown. A trailing
/// `-preview` is stripped before matching, so `gemini-3-flash-preview`
/// matches its entry directly and variants of it fall through.
pub fn model_pricing(id: &str) -> Option<Pricing> {
    let table: &[(&str, Pricing)] = &[
        // flash-latest aliases the newest flash family (3.8 today)
        (
            "gemini-flash-latest",
            Pricing::with_cached_input(0.75, 3.75),
        ),
        (
            "gemini-flash-lite-latest",
            Pricing::with_cached_input(0.30, 2.50),
        ),
        ("gemini-3.8-flash", Pricing::with_cached_input(0.75, 3.75)),
        ("gemini-3.7-flash", Pricing::with_cached_input(0.75, 3.75)),
        ("gemini-3.6-flash", Pricing::with_cached_input(0.75, 3.75)),
        ("gemini-3.5-flash", Pricing::with_cached_input(1.50, 9.00)),
        (
            "gemini-3.5-flash-lite",
            Pricing::with_cached_input(0.30, 2.50),
        ),
        (
            "gemini-3.1-flash-lite",
            Pricing::with_cached_input(0.25, 1.50),
        ),
        (
            "gemini-3-flash-preview",
            Pricing::with_cached_input(0.50, 3.00),
        ),
        (
            "gemini-3.1-pro-preview",
            Pricing::with_cached_input(2.00, 12.00),
        ),
        ("gemini-2.5-pro", Pricing::with_cached_input(1.25, 10.00)),
        ("gemini-2.5-flash", Pricing::with_cached_input(0.30, 2.50)),
        (
            "gemini-2.5-flash-lite",
            Pricing::with_cached_input(0.10, 0.40),
        ),
    ];
    let trimmed = id.strip_suffix("-preview").unwrap_or(id);
    table
        .iter()
        .find(|(entry, _)| *entry == trimmed || *entry == id)
        .map(|(_, pricing)| *pricing)
}

impl GeminiModel {
    /// The model's price, when this crate knows it: a pricing carried by the
    /// model row itself wins, the curated table by id falls back.
    pub fn pricing(&self) -> Option<Pricing> {
        self.pricing.or_else(|| model_pricing(&self.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_price_and_variants_fall_back() {
        let pricing = model_pricing("gemini-3.8-flash").unwrap();
        assert_eq!(pricing.input_per_mtok, 0.75);
        assert_eq!(pricing.output_per_mtok, 3.75);
        // A -preview variant of a listed id still prices.
        let pricing = model_pricing("gemini-3.8-flash-preview").unwrap();
        assert_eq!(pricing.input_per_mtok, 0.75);
        // The aliases price like the family they point at.
        assert_eq!(
            model_pricing("gemini-flash-latest").unwrap(),
            model_pricing("gemini-3.8-flash").unwrap()
        );
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert_eq!(model_pricing("gemma-4-26b-a4b-it"), None);
        assert_eq!(model_pricing("antigravity-preview-05-2026"), None);
    }

    #[test]
    fn instance_pricing_wins_and_the_table_falls_back() {
        // A listed id with no instance pricing still prices from the table.
        let from_table = GeminiModel {
            id: "gemini-3.8-flash".into(),
            display_name: "Gemini 3.8 Flash".into(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec![],
            pricing: None,
        };
        assert_eq!(from_table.pricing(), model_pricing("gemini-3.8-flash"));

        // An instance price beats the table, even for a listed id.
        let instance = Pricing {
            input_per_mtok: 9.0,
            output_per_mtok: 9.0,
            cached_input_per_mtok: Some(2.25),
        };
        let priced = GeminiModel {
            pricing: Some(instance),
            ..from_table
        };
        assert_eq!(priced.pricing(), Some(instance));
    }
}
