//! Live smoke test against the real OpenRouter API. Ignored by default —
//! CI and plain `cargo test` never hit the network.
//!
//! Run with:
//!
//! ```sh
//! OPENROUTER_API_KEY=... cargo test -p rhema-provider-openrouter --all-features -- --ignored
//! ```

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role,
};
use rhema_provider::ModelListing;
use rhema_provider_openrouter::OpenRouterProvider;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// The live key; set `OPENROUTER_API_KEY` to run these tests.
fn live_key() -> String {
    std::env::var("OPENROUTER_API_KEY")
        .unwrap_or_else(|_| panic!("set OPENROUTER_API_KEY to run the live test"))
}

/// The model a trivial completion is asked of; override for variety.
fn live_model() -> String {
    std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-4o-mini".to_owned())
}

#[tokio::test]
#[ignore = "hits the live OpenRouter API; set OPENROUTER_API_KEY and run with `cargo test -p rhema-provider-openrouter --all-features -- --ignored`"]
async fn live_openrouter_lists_its_catalog() {
    let models = OpenRouterProvider::new(live_key())
        .list_models()
        .await
        .expect("the live models call succeeds");
    assert!(
        models.len() > 100,
        "the OpenRouter catalog lists hundreds of models, got {}",
        models.len()
    );
    // Slug shape, a context ceiling, and a converted price on a typical
    // entry.
    let priced = models
        .iter()
        .find(|model| model.pricing.is_some())
        .expect("at least one catalog entry carries pricing");
    assert!(priced.id.contains('/'), "got {priced:?}");
    let pricing = priced.pricing.expect("checked above");
    assert!(
        pricing.input_per_mtok < 1000.0,
        "per-token strings must convert to sane per-MTok prices, got {pricing:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live OpenRouter API; set OPENROUTER_API_KEY and run with `cargo test -p rhema-provider-openrouter --all-features -- --ignored`"]
async fn live_openrouter_answers_a_trivial_prompt() {
    let provider = OpenRouterProvider::new(live_key());
    let request = ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "Reply with exactly: ok")],
        tools: vec![],
        params: ChatParams::new(live_model()),
    };
    let mut sink = NullSink;
    let out = provider
        .complete(request, &mut sink)
        .await
        .expect("the live streaming call succeeds");

    let text = out
        .message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        !text.trim().is_empty(),
        "expected non-empty text, got {out:?}"
    );
    assert!(
        out.usage.input_tokens > 0,
        "the final usage chunk carries token accounting, got {:?}",
        out.usage
    );
}
