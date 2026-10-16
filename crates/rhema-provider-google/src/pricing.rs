//! Pay-as-you-go prices for the chat models, USD per 1M tokens.
//!
//! The ListModels API carries no pricing, so this is a curated table.
//! Source: ai.google.dev/gemini-api/docs/pricing (fetched 2026-10-06);
//! pro entries are the ≤200K-token tier, and the 3.6–3.8 flash prices are
//! the promo rate valid through 2026-12-31. Prices drift — update here
//! when they do; unknown models simply show no cost.

use rhema_provider::Pricing;

/// A price pair at Gemini's implicit-cache discount: cached input
/// reads at a quarter of base input.
fn with_cached_input(input_per_mtok: f64, output_per_mtok: f64) -> Pricing {
    Pricing {
        input_per_mtok,
        output_per_mtok,
        cached_input_per_mtok: Some(input_per_mtok * 0.25),
    }
}

/// The table entry for `id`, or `None` when unknown. A trailing
/// `-preview` is stripped before matching, so `gemini-3-flash-preview`
/// matches its entry directly and variants of it fall through.
pub fn model_pricing(id: &str) -> Option<Pricing> {
    let table: &[(&str, Pricing)] = &[
        // flash-latest aliases the newest flash family (3.8 today)
        ("gemini-flash-latest", with_cached_input(0.75, 3.75)),
        ("gemini-flash-lite-latest", with_cached_input(0.30, 2.50)),
        ("gemini-3.8-flash", with_cached_input(0.75, 3.75)),
        ("gemini-3.7-flash", with_cached_input(0.75, 3.75)),
        ("gemini-3.6-flash", with_cached_input(0.75, 3.75)),
        ("gemini-3.5-flash", with_cached_input(1.50, 9.00)),
        ("gemini-3.5-flash-lite", with_cached_input(0.30, 2.50)),
        ("gemini-3.1-flash-lite", with_cached_input(0.25, 1.50)),
        ("gemini-3-flash-preview", with_cached_input(0.50, 3.00)),
        ("gemini-3.1-pro-preview", with_cached_input(2.00, 12.00)),
        ("gemini-2.5-pro", with_cached_input(1.25, 10.00)),
        ("gemini-2.5-flash", with_cached_input(0.30, 2.50)),
        ("gemini-2.5-flash-lite", with_cached_input(0.10, 0.40)),
    ];
    let trimmed = id.strip_suffix("-preview").unwrap_or(id);
    table
        .iter()
        .find(|(entry, _)| *entry == trimmed || *entry == id)
        .map(|(_, pricing)| *pricing)
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
