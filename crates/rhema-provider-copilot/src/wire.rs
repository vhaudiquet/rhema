//! Copilot's slice of the OpenAI wire: the internal-user endpoint
//! parsing and the model-to-endpoint routing that are Copilot's alone.
//!
//! The shared chat-completions vocabulary, request building, chunk
//! assembly, the SSE drain, lives in
//! [`rhema_provider_openai_compatible`], and Copilot speaks that crate's
//! own [`OPENAI_COMPATIBLE`](rhema_provider_openai_compatible::wire::OPENAI_COMPATIBLE)
//! dialect verbatim: the proxy takes the OpenAI spellings of effort and
//! reasoning replay as-is. Everything else, roles, image parts,
//! tool-call fragment assembly, the `[DONE]` sentinel, is the shared
//! dialect's behavior, documented there. The newer models the proxy
//! serves only on `POST {api}/responses` speak that crate's Responses
//! wire too; `model_route` reads the models page's
//! `supported_endpoints` to route them.
//!
//! What remains here is the seat side: `GET {api}/copilot_internal/user`,
//! GitHub's internal (undocumented, editor-load-bearing) seat endpoint,
//! from which only the premium quota this crate needs is parsed, every
//! field defaulting, so a shape drift maps to no allowance, never to a
//! wrong badge.

use rhema_provider::{Allowance, ModelCard};
use serde::Deserialize;
use serde_json::Value;

// --- Allowance: the internal user endpoint -> normalized ---

/// The `GET {api}/copilot_internal/user` response. The endpoint is
/// GitHub's internal one, undocumented, but stable enough that editor
/// integrations build on it, so only the premium quota this crate
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
                // Every kept row is a chat model; which endpoint serves
                // it is the provider's route, not a generation method.
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
                family: None,
                release_date: None,
            })
        })
        .collect()
}

/// The chat endpoint a model is served on, as the models page's
/// `supported_endpoints` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRoute {
    /// `POST {api}/chat/completions`.
    ChatCompletions,
    /// `POST {api}/responses`, named without chat completions: the newer
    /// OpenAI models the proxy serves on the Responses API alone.
    Responses,
}

/// The route the models page names for one model entry, `None` when the
/// entry carries no endpoints. A model that names both keeps chat
/// completions: it answers there, and only the chat endpoint's own
/// rejection moves it.
pub(crate) fn model_route(entry: &Value) -> Option<ChatRoute> {
    let endpoints = entry.get("supported_endpoints")?.as_array()?;
    let includes = |endpoint: &str| {
        endpoints
            .iter()
            .any(|value| value.as_str() == Some(endpoint))
    };
    if includes("/responses") && !includes("/chat/completions") {
        Some(ChatRoute::Responses)
    } else if includes("/chat/completions") {
        Some(ChatRoute::ChatCompletions)
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::{ChatRoute, model_route};
    use serde_json::json;

    #[test]
    fn the_models_page_names_the_endpoint_that_serves_each_model() {
        let responses_only = json!({
            "id": "gpt-6-luna",
            "supported_endpoints": ["/responses"]
        });
        assert_eq!(model_route(&responses_only), Some(ChatRoute::Responses));

        // Both endpoints: chat completions is the one that answers, and
        // there is no reason to move a working model.
        let both = json!({
            "id": "gpt-5.4",
            "supported_endpoints": ["/chat/completions", "/responses"]
        });
        assert_eq!(model_route(&both), Some(ChatRoute::ChatCompletions));

        let chat_only = json!({
            "id": "gpt-4.1",
            "supported_endpoints": ["/chat/completions"]
        });
        assert_eq!(model_route(&chat_only), Some(ChatRoute::ChatCompletions));

        // The Messages-only Claude rows never reach this route, and a
        // page without endpoints leaves the provider on its default.
        let messages = json!({
            "id": "claude-sonnet-4-5",
            "supported_endpoints": ["/v1/messages"]
        });
        assert_eq!(model_route(&messages), None);
        assert_eq!(model_route(&json!({"id": "gpt-4.1"})), None);
    }
}
