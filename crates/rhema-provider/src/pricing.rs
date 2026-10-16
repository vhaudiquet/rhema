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
