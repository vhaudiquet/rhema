//! Copilot's slice of the OpenAI chat-completions wire: only the
//! internal-user endpoint parsing that is Copilot's alone.
//!
//! The shared chat-completions vocabulary — request building, chunk
//! assembly, the SSE drain — lives in
//! [`rhema_provider_openai_compatible`], and Copilot speaks that crate's
//! own [`OPENAI_COMPATIBLE`](rhema_provider_openai_compatible::wire::OPENAI_COMPATIBLE)
//! dialect verbatim: the proxy takes the OpenAI spellings of effort and
//! reasoning replay as-is. Everything else — roles, image parts,
//! tool-call fragment assembly, the `[DONE]` sentinel — is the shared
//! dialect's behavior, documented there.
//!
//! What remains here is the seat side: `GET {api}/copilot_internal/user`,
//! GitHub's internal (undocumented, editor-load-bearing) seat endpoint,
//! from which only the premium quota this crate needs is parsed — every
//! field defaulting, so a shape drift maps to no allowance, never to a
//! wrong badge.

use rhema_provider::{Allowance, ModelCard};
use serde::Deserialize;
use serde_json::Value;

// --- Allowance: the internal user endpoint -> normalized ---

/// The `GET {api}/copilot_internal/user` response. The endpoint is
/// GitHub's internal one — undocumented, but stable enough that editor
/// integrations build on it — so only the premium quota this crate
/// needs is parsed, every field defaulting: a shape drift maps to no
/// allowance, never to a wrong badge.
#[derive(Debug, Default, Deserialize)]
pub struct WireCopilotUser {
    /// The seat's per-kind quota snapshots, when the response has them.
    #[serde(default)]
    pub quota_snapshots: Option<WireQuotaSnapshots>,
}

/// The `quota_snapshots` object of a [`WireCopilotUser`]. The chat and
/// completions snapshots are ignored: a coding agent's spend is the
/// premium one.
#[derive(Debug, Default, Deserialize)]
pub struct WireQuotaSnapshots {
    /// The premium-request quota of the plan cycle.
    #[serde(default)]
    pub premium_interactions: Option<WireQuotaSnapshot>,
}

/// One quota snapshot: the cycle's cap and what is left of it.
#[derive(Debug, Default, Deserialize)]
pub struct WireQuotaSnapshot {
    /// The requests the cycle allots, when the seat is capped.
    #[serde(default)]
    pub entitlement: Option<f64>,
    /// The requests still unspent.
    #[serde(default)]
    pub remaining: Option<f64>,
}

/// Normalizes the models page: keeps the entries the proxy marks as chat
/// capable, carrying their context and output limits. A subscription
/// serves the tokens, so no card carries a price.
pub fn normalize_models(page: &serde_json::Value) -> Vec<ModelCard> {
    let entries = page
        .get("data")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let capabilities = entry.get("capabilities");
            if capabilities
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                != Some("chat")
            {
                return None;
            }
            let limits = capabilities.and_then(|c| c.get("limits"));
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Some(ModelCard {
                id,
                display_name: text("name"),
                description: String::new(),
                input_token_limit: limits
                    .and_then(|l| l.get("max_context_window_tokens"))
                    .and_then(Value::as_u64),
                output_token_limit: limits
                    .and_then(|l| l.get("max_output_tokens"))
                    .and_then(Value::as_u64),
                supports_vision: None,
                // Every kept model is offered through chat completions.
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
            })
        })
        .collect()
}

/// The seat's premium-request allowance: `remaining` out of the cycle's
/// `entitlement`, the cap dropped when the seat reports none. `None`
/// when the response carries no premium snapshot.
pub fn allowance(user: &WireCopilotUser) -> Option<Allowance> {
    let snapshot = user
        .quota_snapshots
        .as_ref()?
        .premium_interactions
        .as_ref()?;
    let remaining = snapshot.remaining?;
    let amount = match snapshot.entitlement {
        Some(cap) if cap > 0.0 => format!("{remaining:.0}/{cap:.0}"),
        _ => format!("{remaining:.0}"),
    };
    Some(Allowance {
        amount,
        kind: "premium".to_owned(),
    })
}
