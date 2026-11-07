//! Live smoke test against a running `llama-server`. Ignored by default —
//! CI and plain `cargo test` never touch the network.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p rhema-provider-llama-cpp --all-features -- --ignored
//! # or, against a non-default server:
//! LLAMA_CPP_URL=http://localhost:8081 cargo test -p rhema-provider-llama-cpp --all-features -- --ignored
//! ```

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role,
};
use rhema_provider::ModelListing;
use rhema_provider_llama_cpp::{DEFAULT_BASE_URL, LlamaCppProvider};

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// The server to hit, overridable for a non-default port.
fn live_url() -> String {
    std::env::var("LLAMA_CPP_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned())
}

#[tokio::test]
#[ignore = "hits a live llama-server; start one and run with `cargo test -p rhema-provider-llama-cpp --all-features -- --ignored`"]
async fn live_llama_server_lists_its_model() {
    let provider = LlamaCppProvider::new(live_url());
    let models = provider
        .list_models()
        .await
        .expect("the live models call succeeds");
    assert!(
        !models.is_empty(),
        "a loaded llama-server lists at least one model, got {models:?}"
    );
}

#[tokio::test]
#[ignore = "hits a live llama-server; start one and run with `cargo test -p rhema-provider-llama-cpp --all-features -- --ignored`"]
async fn live_llama_server_answers_a_trivial_prompt() {
    let provider = LlamaCppProvider::new(live_url());
    let request = ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "Reply with exactly: ok")],
        tools: vec![],
        params: ChatParams::new("local"),
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
        "the include_usage final chunk carries usage, got {:?}",
        out.usage
    );
}
