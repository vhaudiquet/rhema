//! Live smoke test against the real Anthropic API. Ignored by default — CI
//! and plain `cargo test` never hit the network.
//!
//! Run with:
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo test -p rhema-provider-anthropic --all-features -- --ignored
//! ```

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role,
};
use rhema_provider::ModelListing;
use rhema_provider_anthropic::{AnthropicProvider, ZAI_BASE_URL};

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// The live key; set `ANTHROPIC_API_KEY` to run these tests.
fn live_key() -> String {
    std::env::var("ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("set ANTHROPIC_API_KEY to run the live test"))
}

/// The model a trivial completion is asked of; override for variety.
fn live_model() -> String {
    std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_owned())
}

#[tokio::test]
#[ignore = "hits the live Anthropic API; set ANTHROPIC_API_KEY and run with `cargo test -p rhema-provider-anthropic --all-features -- --ignored`"]
async fn live_anthropic_lists_its_models() {
    let models = AnthropicProvider::new(live_key())
        .list_models()
        .await
        .expect("the live models call succeeds");
    assert!(
        models.iter().any(|model| model.id.starts_with("claude-")),
        "expected at least one claude model, got {models:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live Anthropic API; set ANTHROPIC_API_KEY and run with `cargo test -p rhema-provider-anthropic --all-features -- --ignored`"]
async fn live_anthropic_answers_a_trivial_prompt() {
    let provider = AnthropicProvider::new(live_key());
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
        "message_start carries token accounting, got {:?}",
        out.usage
    );
}

/// The Z.ai Coding Plan key; set `ZAI_API_KEY` (or `Z_AI_API_KEY`) to run
/// these tests.
fn zai_key() -> String {
    std::env::var("ZAI_API_KEY")
        .or_else(|_| std::env::var("Z_AI_API_KEY"))
        .unwrap_or_else(|_| panic!("set ZAI_API_KEY to run the Z.ai live test"))
}

/// The Z.ai model a trivial completion is asked of; override for variety.
fn zai_model() -> String {
    std::env::var("ZAI_MODEL").unwrap_or_else(|_| "GLM-5.3".to_owned())
}

/// The Coding Plan provider: bearer token auth against the
/// Anthropic-compatible route.
fn zai_provider() -> AnthropicProvider {
    AnthropicProvider::new(zai_key())
        .with_base_url(ZAI_BASE_URL)
        .with_bearer_auth()
}

#[tokio::test]
#[ignore = "hits the live Z.ai Coding Plan endpoint; set ZAI_API_KEY and run with `cargo test -p rhema-provider-anthropic --all-features -- --ignored`"]
async fn live_zai_lists_the_coding_plan_catalog() {
    let models = zai_provider()
        .list_models()
        .await
        .expect("the Z.ai models call succeeds");
    assert!(
        models
            .iter()
            .any(|model| model.id.to_lowercase().contains("glm")),
        "expected at least one glm model, got {models:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live Z.ai Coding Plan endpoint; set ZAI_API_KEY and run with `cargo test -p rhema-provider-anthropic --all-features -- --ignored`"]
async fn live_zai_answers_a_trivial_prompt() {
    let request = ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "Reply with exactly: ok")],
        tools: vec![],
        params: ChatParams::new(zai_model()),
    };
    let mut sink = NullSink;
    let out = zai_provider()
        .complete(request, &mut sink)
        .await
        .expect("the Z.ai streaming call succeeds");

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
        "message_start carries token accounting, got {:?}",
        out.usage
    );
}

#[tokio::test]
#[ignore = "hits the live Z.ai Coding Plan endpoint; set ZAI_API_KEY and run with `cargo test -p rhema-provider-anthropic --all-features -- --ignored`"]
async fn live_zai_reports_the_plan_windows() {
    let allowance = zai_provider()
        .allowance()
        .await
        .expect("the live quota call succeeds")
        .expect("the plan reports at least one coding window");
    assert!(
        allowance.amount.contains("5h"),
        "the 5-hour window rides the badge, got {allowance:?}"
    );
}
