//! Live smoke test against the real endpoints. Ignored by default — CI
//! and plain `cargo test` never hit the network.
//!
//! Run with:
//!
//! ```sh
//! GITHUB_COPILOT_TOKEN=... cargo test -p rhema-provider-copilot --all-features -- --ignored
//! ```
//!
//! The token is the device flow's output (`rhema login` stores it); it
//! must belong to a seat with an active Copilot subscription.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role,
};
use rhema_provider::ModelListing;
use rhema_provider_copilot::CopilotProvider;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// The live GitHub token; set `GITHUB_COPILOT_TOKEN` to run these tests.
fn live_key() -> String {
    std::env::var("GITHUB_COPILOT_TOKEN")
        .unwrap_or_else(|_| panic!("set GITHUB_COPILOT_TOKEN to run the live test"))
}

/// The model a trivial completion is asked of; override for variety.
fn live_model() -> String {
    std::env::var("COPILOT_MODEL").unwrap_or_else(|_| "gpt-4.1".to_owned())
}

#[tokio::test]
#[ignore = "hits the live Copilot endpoints; set GITHUB_COPILOT_TOKEN and run with `cargo test -p rhema-provider-copilot --all-features -- --ignored`"]
async fn live_copilot_lists_its_models() {
    let models = CopilotProvider::new(live_key())
        .list_models()
        .await
        .expect("the live models call succeeds");
    assert!(
        !models.is_empty(),
        "expected at least one chat model, got {models:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live Copilot endpoints; set GITHUB_COPILOT_TOKEN and run with `cargo test -p rhema-provider-copilot --all-features -- --ignored`"]
async fn live_copilot_answers_a_trivial_prompt() {
    let provider = CopilotProvider::new(live_key());
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
}
