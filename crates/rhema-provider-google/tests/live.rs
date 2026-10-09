//! Live smoke test against the real Gemini API. Ignored by default — CI and
//! plain `cargo test` never hit the network.
//!
//! Run with:
//!
//! ```sh
//! GEMINI_API_KEY=... cargo test -p rhema-provider-google --all-features -- --ignored
//! ```

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role,
};
use rhema_provider_google::GoogleProvider;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

#[tokio::test]
#[ignore = "hits the live Gemini API; set GEMINI_API_KEY and run with `cargo test -p rhema-provider-google --all-features -- --ignored`"]
async fn live_gemini_lists_text_chat_models() {
    let key = std::env::var("GEMINI_API_KEY")
        .unwrap_or_else(|_| panic!("set GEMINI_API_KEY to run the live test"));

    let models = GoogleProvider::new(key)
        .list_models()
        .await
        .expect("the live ListModels call succeeds");
    assert!(
        models.iter().any(|model| model.id.starts_with("gemini-")),
        "expected at least one text chat gemini model, got {models:?}"
    );
}

#[tokio::test]
#[ignore = "hits the live Gemini API; set GEMINI_API_KEY and run with `cargo test -p rhema-provider-google --all-features -- --ignored`"]
async fn live_gemini_answers_a_trivial_prompt() {
    let key = std::env::var("GEMINI_API_KEY")
        .unwrap_or_else(|_| panic!("set GEMINI_API_KEY to run the live test"));

    let provider = GoogleProvider::new(key);
    let request = ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "Reply with exactly: ok")],
        tools: vec![],
        params: ChatParams::new("gemini-3.8-flash"),
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
