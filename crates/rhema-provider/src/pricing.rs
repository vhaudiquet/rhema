//! The per-MTok price record adapters attach to catalog rows.

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
    /// What one model call costs at these rates, from the tokens it
    /// bills: input, output, and the cached share of the input.
    ///
    /// Cached prompt tokens bill at a fraction of base input
    /// everywhere — Anthropic reads ~10%, OpenAI 50%, DeepSeek and
    /// Gemini ~25%; the cross-provider middle stands in where no exact
    /// rate is published. Cache writes ride the full input bucket.
    pub fn cost(&self, input_tokens: u64, output_tokens: u64, cached_input_tokens: u64) -> f64 {
        const CACHED_INPUT_FALLBACK: f64 = 0.25;
        let cached_rate = self
            .cached_input_per_mtok
            .unwrap_or(self.input_per_mtok * CACHED_INPUT_FALLBACK);
        let uncached = input_tokens.saturating_sub(cached_input_tokens);
        uncached as f64 / 1_000_000.0 * self.input_per_mtok
            + cached_input_tokens as f64 / 1_000_000.0 * cached_rate
            + output_tokens as f64 / 1_000_000.0 * self.output_per_mtok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_prices_uncached_cached_and_output_tokens() {
        let pricing = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 3.0,
            cached_input_per_mtok: None,
        };
        // 600 uncached at $1/M, 400 cached at the quarter fallback,
        // 100 output at $3/M.
        let cost = pricing.cost(1_000, 100, 400);
        assert!((cost - 0.001).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn a_published_cached_rate_beats_the_fallback() {
        let pricing = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cached_input_per_mtok: Some(0.1),
        };
        // All 1_000 input tokens cached at the published tenth.
        let cost = pricing.cost(1_000, 0, 1_000);
        assert!((cost - 0.0001).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn cached_tokens_beyond_the_prompt_still_bill_as_cached() {
        let pricing = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            cached_input_per_mtok: None,
        };
        // Uncached saturates to zero; the 400 cached tokens bill at
        // the quarter fallback.
        let cost = pricing.cost(100, 0, 400);
        assert!((cost - 0.0001).abs() < 1e-9, "{cost}");
    }
}
