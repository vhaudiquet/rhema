//! OpenRouter's dialect of the OpenAI chat-completions wire, plus the
//! catalog and account parsing that is OpenRouter's alone.
//!
//! The shared chat-completions vocabulary, request building, chunk
//! assembly, the SSE drain, lives in
//! [`rhema_provider_openai_compatible`]; this crate teaches it the one
//! spelling OpenRouter does differently and reuses the rest:
//!
//! - Every reasoning field is named `reasoning`, and effort rides as an
//!   object, `{"reasoning": {"effort": "high"}}` on the request, and
//!   the chain of thought replayed under `reasoning` on assistant turns
//!   (not OpenAI's `reasoning_effort` string / `reasoning_content`).
//! - A malformed streamed arguments payload wraps as
//!   `{"_openrouter_args_text": <raw>}` so the call still round-trips.
//! - A request-root `cache_control: {"type": "ephemeral"}` rides on
//!   every request for the vendors that cache solely on explicit
//!   request (Anthropic and Alibaba's Qwen, `cache_control_directive`);
//!   both chat-completions and Responses bodies carry it, the
//!   breakpoint the router translates rides the last cacheable block
//!   and advances as the conversation grows. Without it those vendors
//!   never cache and report no `cached_tokens`, which is how a whole
//!   session can bill millions of input tokens at full price. The
//!   usage accounting comes back on
//!   `prompt_tokens_details.cached_tokens` (chat) /
//!   `input_tokens_details.cached_tokens` (Responses) either way, and
//!   the shared mapping reads both.
//! - The conversation id rides as the request-root `session_id`
//!   whenever the request carries one ([`ChatParams::session_id`]).
//!   The key is OpenRouter's sticky-routing opt-in: with it, the
//!   provider that served the conversation keeps serving it from the
//!   first request on (without it sticky routing waits for a first
//!   cache hit, and a conversation whose opening messages change
//!   between requests hashes to new buckets), and OpenRouter's Logs
//!   group one key's requests under one session.
//!
//! Everything else, roles, content parts for images, tool-call
//! fragment assembly by index, the `[DONE]` sentinel, usage on the
//! final chunk, is the shared dialect's behavior, documented there.
//!
//! What remains here is the catalog side (paginated `/models` with
//! per-token pricing and modality facts) and the account side
//! (`/credits` + `/auth/key` headroom).
//!
//! [`ChatParams::session_id`]: rhema_core::request::ChatParams::session_id

use rhema_provider::{Allowance, ModelCard, Pricing};
use rhema_provider_openai_compatible::wire::Dialect;
use serde::Deserialize;
use serde_json::{Value, json};

/// OpenRouter's dialect: the shared vocabulary with every reasoning
/// field renamed `reasoning`, effort as an object.
pub const OPENROUTER: Dialect = Dialect {
    args_key: "_openrouter_args_text",
    assistant_reasoning_key: Some("reasoning"),
    replays_reasoning_as_thinking: false,
    effort_key: "reasoning",
    // Usage rides every stream unconditionally; the opt-in flag is
    // deprecated on OpenRouter.
    sends_stream_options: false,
    sends_session_id: true,
    sends_prompt_cache_key: rhema_provider_openai_compatible::wire::PromptCacheKeyRoute::None,
    reasoning_effort: reasoning_effort_value,
    automatic_cache_control: cache_control_directive,
};

/// The OpenRouter wire spelling of a thinking level: an effort object
/// under `reasoning`; `None` sends nothing and the default stands.
fn reasoning_effort_value(thinking: rhema_core::request::ThinkingEffort) -> Option<Value> {
    let effort = match thinking {
        rhema_core::request::ThinkingEffort::Default => return None,
        rhema_core::request::ThinkingEffort::Low => "low",
        rhema_core::request::ThinkingEffort::Medium => "medium",
        rhema_core::request::ThinkingEffort::High => "high",
    };
    Some(json!({ "effort": effort }))
}

/// The request-root `cache_control` a model id needs: OpenRouter's
/// automatic-caching directive for the vendors that cache solely on
/// explicit request, Anthropic and Alibaba's Qwen being the two
/// OpenRouter itself calls out (their usage carries
/// `prompt_tokens_details.cached_tokens` only once the opt-in rides,
/// and Anthropic bills the whole prompt uncached without it). The
/// directive marks the request root, the breakpoint rides the last
/// cacheable block and advances as the conversation grows, which is
/// the shape an agent loop already has. Every other vendor caches
/// implicitly, so nothing is sent, a dead field on OpenAI or DeepSeek
/// routes buys nothing and risks an odd lookup.
fn cache_control_directive(model: &str) -> Option<Value> {
    const NEEDS_OPT_IN: [&str; 2] = ["anthropic/", "qwen/"];
    NEEDS_OPT_IN
        .iter()
        .any(|vendor| model.starts_with(vendor))
        .then(|| json!({ "type": "ephemeral" }))
}

/// Whether the model is a text chat model. Every listed row is served
/// through chat completions, but the catalog aggregates image, audio,
/// speech, and embedding backends whose ids carry the modality; those
/// cannot drive a coding agent and are excluded by id.
pub fn is_text(model: &ModelCard) -> bool {
    const NON_TEXT: [&str; 9] = [
        "image",
        "audio",
        "tts",
        "asr",
        "transcribe",
        "whisper",
        "embedding",
        "moderation",
        "video",
    ];
    !NON_TEXT.iter().any(|marker| model.id.contains(marker))
}

/// One page of the `GET /models` response.
#[derive(Debug, Default, Deserialize)]
pub struct WireModelsPage {
    /// The models on this page.
    #[serde(default)]
    pub data: Vec<WireModel>,
    /// Paging links; `next` is null on the last page.
    #[serde(default)]
    pub links: Option<WirePageLinks>,
}

/// The `links` object of a [`WireModelsPage`].
#[derive(Debug, Default, Deserialize)]
pub struct WirePageLinks {
    /// Path of the next page, when there is one.
    #[serde(default)]
    pub next: Option<String>,
}

/// One wire model object. Dozens of catalog fields exist; only the ones a
/// picker row shows are kept.
#[derive(Debug, Default, Deserialize)]
pub struct WireModel {
    /// Model slug (e.g. `openai/gpt-4o-mini`), usable as the request's
    /// `model` value.
    #[serde(default)]
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// What the model is for.
    #[serde(default)]
    pub description: String,
    /// Max context in tokens, when advertised.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// The entry's modality facts, when advertised.
    #[serde(default)]
    pub architecture: Option<WireArchitecture>,
    /// Prices in USD per single token, when advertised.
    #[serde(default)]
    pub pricing: Option<WirePricing>,
}

/// The `architecture` object of a [`WireModel`]: what goes in, what comes
/// out. The only field a picker row needs is the input modality list.
#[derive(Debug, Default, Deserialize)]
pub struct WireArchitecture {
    /// The input modalities the model accepts (e.g. `["text", "image"]`),
    /// when the entry publishes them.
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
}

/// The `pricing` object of a [`WireModel`]; the per-request, image, and
/// audio prices are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct WirePricing {
    /// USD per prompt token, as a decimal string.
    #[serde(default)]
    pub prompt: Option<String>,
    /// USD per completion token, as a decimal string.
    #[serde(default)]
    pub completion: Option<String>,
    /// USD per prompt token served from the provider's cache, when the
    /// upstream supports prompt caching.
    #[serde(default)]
    pub input_cache_read: Option<String>,
}

impl WireModelsPage {
    /// Whether another page follows: `links.next` present.
    pub fn next_page(&self) -> bool {
        self.links
            .as_ref()
            .is_some_and(|links| links.next.is_some())
    }
}

/// A USD-per-token string as a plain number; absent, unparseable, and
/// negative (sentinel) values all mean "not priced".
fn token_price(raw: Option<&str>) -> Option<f64> {
    let value: f64 = raw?.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// The model's per-MTok price, when its entry prices both directions.
/// The catalog's cache-read rate rides along when published.
fn per_mtok_pricing(wire: &WireModel) -> Option<Pricing> {
    let pricing = wire.pricing.as_ref()?;
    let cached_input_per_mtok =
        token_price(pricing.input_cache_read.as_deref()).map(|rate| rate * 1_000_000.0);
    Some(Pricing {
        input_per_mtok: token_price(pricing.prompt.as_deref())? * 1_000_000.0,
        output_per_mtok: token_price(pricing.completion.as_deref())? * 1_000_000.0,
        cached_input_per_mtok,
    })
}

/// The listing's own vision verdict: an entry publishing its input
/// modalities names `image` there or not, a real `Some` either way. No
/// published list (`architecture` absent, or the field absent from it)
/// stays unknown rather than guessed.
fn vision_of(wire: &WireModel) -> Option<bool> {
    wire.architecture
        .as_ref()?
        .input_modalities
        .as_ref()
        .map(|modalities| modalities.iter().any(|modality| modality == "image"))
}

impl From<WireModel> for ModelCard {
    fn from(wire: WireModel) -> Self {
        // Priced and judged before the fields move out.
        let pricing = per_mtok_pricing(&wire);
        let supports_vision = vision_of(&wire);
        Self {
            id: wire.id,
            display_name: wire.name,
            description: wire.description,
            input_token_limit: wire.context_length,
            // OpenRouter shares the context window between directions and
            // reports no output ceiling.
            output_token_limit: None,
            // Every listed model is offered through chat completions.
            supports_vision,
            generation_methods: vec!["generateContent".to_owned()],
            pricing,
            family: None,
            release_date: None,
        }
    }
}

// --- Allowance: account endpoints -> normalized ---

/// One page of the `GET /credits` response: the account's prepaid
/// credit and what it has spent.
#[derive(Debug, Default, Deserialize)]
pub struct WireCredits {
    /// The credit figures; `None` when the body is an error envelope.
    #[serde(default)]
    pub data: Option<WireCreditsData>,
}

/// The `data` object of a [`WireCredits`].
#[derive(Debug, Default, Deserialize)]
pub struct WireCreditsData {
    /// Total credit ever granted the account, in USD.
    #[serde(default)]
    pub total_credits: Option<f64>,
    /// What the account has spent so far, in USD.
    #[serde(default)]
    pub total_usage: f64,
}

/// The `GET /auth/key` response: the calling key's own cap and spend.
#[derive(Debug, Default, Deserialize)]
pub struct WireKey {
    /// The key figures; `None` when the body is an error envelope.
    #[serde(default)]
    pub data: Option<WireKeyData>,
}

/// The `data` object of a [`WireKey`].
#[derive(Debug, Default, Deserialize)]
pub struct WireKeyData {
    /// The credit cap this key may spend, in USD; `None` means the key
    /// is limited only by the account.
    #[serde(default)]
    pub limit: Option<f64>,
    /// What the key has spent so far, in USD.
    #[serde(default)]
    pub usage: f64,
}

/// The allowance the account endpoints report: the prepaid credit
/// still available, bounded by the key's own cap when it has one,
/// spending stops at the smaller of the two. `None` when the credits
/// body carries no total; an unbounded key just lifts the cap.
pub fn allowance(credits: &WireCredits, key: &WireKey) -> Option<Allowance> {
    let data = credits.data.as_ref()?;
    let account_left = (data.total_credits? - data.total_usage).max(0.0);
    let key_left = key
        .data
        .as_ref()
        .and_then(|data| data.limit.map(|limit| (limit - data.usage).max(0.0)));
    let left = key_left.map_or(account_left, |key_left| account_left.min(key_left));
    Some(Allowance {
        amount: format!("${left:.2}"),
        kind: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::message::{Content, Message, Role};
    use rhema_core::request::{ChatParams, ModelRequest, ThinkingEffort};
    use rhema_core::tool::ToolCall;
    use rhema_provider_openai_compatible::responses::request_body as responses_request_body;
    use rhema_provider_openai_compatible::wire::request_body;
    use serde_json::json;

    #[test]
    fn a_named_thinking_effort_rides_in_reasoning_effort() {
        let mut req = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams {
                model: "z-ai/glm-5.3".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::High,
                top_p: None,
                stop: Vec::new(),
                session_id: None,
            },
        };
        let body = serde_json::to_string(&request_body(&req, &OPENROUTER)).unwrap();
        assert!(
            body.contains("\"reasoning\":{\"effort\":\"high\"}"),
            "{body}"
        );

        // Default sends nothing: the routed model's default stands.
        req.params.thinking = ThinkingEffort::Default;
        let body = serde_json::to_string(&request_body(&req, &OPENROUTER)).unwrap();
        assert!(!body.contains("\"reasoning\""), "{body}");
    }
    #[test]
    fn assistant_reasoning_replays_from_provider_data() {
        let reasoning = "I should list the files first";
        let req = ModelRequest {
            system: None,
            messages: vec![Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_0".into(),
                    name: "a".into(),
                    args: json!({}),
                    provider_data: json!({ "reasoning": reasoning }),
                })],
            }],
            tools: Vec::new(),
            params: ChatParams {
                model: "z-ai/glm-5.3".into(),
                temperature: None,
                max_tokens: None,
                thinking: ThinkingEffort::Default,
                top_p: None,
                stop: Vec::new(),
                session_id: None,
            },
        };
        let body = serde_json::to_value(request_body(&req, &OPENROUTER)).expect("body serializes");
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning"], reasoning);

        // Without stored reasoning the field stays absent.
        let plain = ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: req.params.clone(),
        };
        let body =
            serde_json::to_value(request_body(&plain, &OPENROUTER)).expect("body serializes");
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("reasoning").is_none()),
            "{body}"
        );
    }

    /// A one-turn request naming `model`, the least body the cache
    /// directive's presence or absence can be read from.
    fn req_for(model: &str) -> ModelRequest {
        ModelRequest {
            system: None,
            messages: vec![Message::text(Role::User, "hello")],
            tools: Vec::new(),
            params: ChatParams::new(model),
        }
    }

    #[test]
    fn an_anthropic_model_requests_automatic_caching() {
        let body = serde_json::to_value(request_body(
            &req_for("anthropic/claude-opus-5.5"),
            &OPENROUTER,
        ))
        .expect("body serializes");
        // The request-root directive, the opt-in Anthropic caches by.
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
    }

    #[test]
    fn a_qwen_model_requests_automatic_caching() {
        let body = serde_json::to_value(request_body(&req_for("qwen/qwen4-max"), &OPENROUTER))
            .expect("body serializes");
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
    }

    #[test]
    fn route_suffixes_change_nothing_about_the_vendor_match() {
        // `:batch`, `:nitro`, `:free` suffixes mark the route, not the
        // vendor; the id still opens with the vendor slug.
        let body = serde_json::to_value(request_body(
            &req_for("anthropic/claude-opus-5.5:batch"),
            &OPENROUTER,
        ))
        .expect("body serializes");
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
    }

    #[test]
    fn an_implicit_caching_model_sends_no_directive() {
        // OpenAI caches by itself; the field would be dead weight, and
        // a vendor the directive doesn't fit must not receive it.
        for model in [
            "openai/gpt-4o-mini",
            "deepseek/deepseek-chat",
            "z-ai/glm-5.3",
        ] {
            let body = serde_json::to_value(request_body(&req_for(model), &OPENROUTER)).unwrap();
            assert!(body.get("cache_control").is_none(), "{model}: {body}");
        }
    }

    #[test]
    fn the_conversation_id_rides_when_the_request_carries_one() {
        let mut req = req_for("anthropic/claude-opus-5.5");
        req.params.session_id = Some("20261120120000-00".into());
        let body = serde_json::to_value(request_body(&req, &OPENROUTER)).expect("body serializes");
        // The sticky-routing key, at the request root where OpenRouter
        // reads it.
        assert_eq!(body["session_id"], "20261120120000-00");

        // No key named, nothing sent: the server falls back to its own
        // conversation identity.
        let body = serde_json::to_value(request_body(
            &req_for("anthropic/claude-opus-5.5"),
            &OPENROUTER,
        ))
        .expect("body serializes");
        assert!(body.get("session_id").is_none(), "{body}");
    }

    #[test]
    fn the_responses_body_carries_the_conversation_id_too() {
        let mut req = req_for("openai/gpt-4o-mini");
        req.params.session_id = Some("20261120120000-00".into());
        let body = serde_json::to_value(responses_request_body(&req, &OPENROUTER))
            .expect("body serializes");
        assert_eq!(body["session_id"], "20261120120000-00");
    }

    #[test]
    fn the_responses_body_carries_the_directive_too() {
        let body = serde_json::to_value(responses_request_body(
            &req_for("anthropic/claude-opus-5.5"),
            &OPENROUTER,
        ))
        .expect("body serializes");
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));

        let body = serde_json::to_value(responses_request_body(
            &req_for("openai/gpt-4o-mini"),
            &OPENROUTER,
        ))
        .expect("body serializes");
        assert!(body.get("cache_control").is_none(), "{body}");
    }
}
