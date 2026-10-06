//! Pay-as-you-go prices for the chat models, USD per 1M tokens.
//!
//! The ListModels API carries no pricing, so this is a curated table.
//! Source: ai.google.dev/gemini-api/docs/pricing (fetched 2026-10-06);
//! pro entries are the ≤200K-token tier, and the 3.6–3.8 flash prices are
//! the promo rate valid through 2026-12-31. Prices drift — update here
//! when they do; unknown models simply show no cost.

use crate::wire::GeminiModel;

/// Input and output price, USD per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    /// Price per 1M input tokens.
    pub input_per_mtok: f64,
    /// Price per 1M output tokens.
    pub output_per_mtok: f64,
}

/// The table entry for `id`, or `None` when unknown. A trailing
/// `-preview` is stripped before matching, so `gemini-3-flash-preview`
/// matches its entry directly and variants of it fall through.
pub fn model_pricing(id: &str) -> Option<Pricing> {
    let table: &[(&str, Pricing)] = &[
        // flash-latest aliases the newest flash family (3.8 today)
        (
            "gemini-flash-latest",
            Pricing {
                input_per_mtok: 0.75,
                output_per_mtok: 3.75,
            },
        ),
        (
            "gemini-flash-lite-latest",
            Pricing {
                input_per_mtok: 0.30,
                output_per_mtok: 2.50,
            },
        ),
        (
            "gemini-3.8-flash",
            Pricing {
                input_per_mtok: 0.75,
                output_per_mtok: 3.75,
            },
        ),
        (
            "gemini-3.7-flash",
            Pricing {
                input_per_mtok: 0.75,
                output_per_mtok: 3.75,
            },
        ),
        (
            "gemini-3.6-flash",
            Pricing {
                input_per_mtok: 0.75,
                output_per_mtok: 3.75,
            },
        ),
        (
            "gemini-3.5-flash",
            Pricing {
                input_per_mtok: 1.50,
                output_per_mtok: 9.00,
            },
        ),
        (
            "gemini-3.5-flash-lite",
            Pricing {
                input_per_mtok: 0.30,
                output_per_mtok: 2.50,
            },
        ),
        (
            "gemini-3.1-flash-lite",
            Pricing {
                input_per_mtok: 0.25,
                output_per_mtok: 1.50,
            },
        ),
        (
            "gemini-3-flash-preview",
            Pricing {
                input_per_mtok: 0.50,
                output_per_mtok: 3.00,
            },
        ),
        (
            "gemini-3.1-pro-preview",
            Pricing {
                input_per_mtok: 2.00,
                output_per_mtok: 12.00,
            },
        ),
        (
            "gemini-2.5-pro",
            Pricing {
                input_per_mtok: 1.25,
                output_per_mtok: 10.00,
            },
        ),
        (
            "gemini-2.5-flash",
            Pricing {
                input_per_mtok: 0.30,
                output_per_mtok: 2.50,
            },
        ),
        (
            "gemini-2.5-flash-lite",
            Pricing {
                input_per_mtok: 0.10,
                output_per_mtok: 0.40,
            },
        ),
    ];
    let trimmed = id.strip_suffix("-preview").unwrap_or(id);
    table
        .iter()
        .find(|(entry, _)| *entry == trimmed || *entry == id)
        .map(|(_, pricing)| *pricing)
}

impl GeminiModel {
    /// The model's price, when this crate knows it.
    pub fn pricing(&self) -> Option<Pricing> {
        model_pricing(&self.id)
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
}
