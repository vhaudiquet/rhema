//! Offline transport tests against a wiremock server standing in for
//! both the ChatGPT backend (Responses, models) and the OAuth token
//! endpoint (refresh).

use rhema_core::event::Event;
use rhema_core::message::{Content, Message, Role};
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ChatParams;
use rhema_provider::ModelListing;
use rhema_provider_chatgpt::{
    ChatGptCredential, ChatGptProvider, DEFAULT_API_BASE, MODELS_CLIENT_VERSION,
};
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A credential whose access token is `access`; `expires_at` far in the
/// future unless the test means to force a refresh.
fn credential(access: &str, refresh: Option<&str>, expires_at: u64) -> String {
    ChatGptCredential {
        access_token: access.to_owned(),
        refresh_token: refresh.map(str::to_owned),
        account_id: Some("acct-1".to_owned()),
        email: None,
        plan: Some("plus".to_owned()),
        expires_at: Some(expires_at),
    }
    .to_json()
}

/// A provider aimed at the mock server, refresh included.
fn provider(server_uri: &str, credential: String) -> ChatGptProvider {
    ChatGptProvider::new(credential)
        .with_api_base(server_uri)
        .with_token_url(format!("{server_uri}/oauth/token"))
}

/// One complete SSE turn: a text delta, a reasoning delta, and the
/// terminal response carrying the authoritative output and usage.
fn sse_body() -> String {
    [
        r#"data: {"type":"response.output_text.delta","delta":"Hel"}"#,
        "",
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        "",
        r#"data: {"type":"response.output_text.delta","delta":"lo"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello"}]}],"usage":{"input_tokens":7,"output_tokens":2,"input_tokens_details":{"cached_tokens":1}}}}"#,
        "",
    ]
    .join("\n")
}

#[tokio::test]
async fn a_turn_streams_through_the_sse_decoder_with_the_backend_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer access"))
        .and(header("chatgpt-account-id", "acct-1"))
        .and(header("originator", "rhema"))
        .and(header("accept", "text/event-stream"))
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-5.1-codex",
            "stream": true,
            "store": false,
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: Some("be terse".into()),
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("gpt-5.1-codex"),
    };
    let mut sink = Collector(vec![]);
    let output = provider(&server.uri(), credential("access", None, 4_102_444_800))
        .complete(request, &mut sink)
        .await
        .expect("the stream completes");

    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    assert_eq!(output.usage.input_tokens, 7);
    assert_eq!(output.usage.cached_input_tokens, 1);
    assert_eq!(
        sink.0,
        vec![
            "TextDelta(\"Hel\")".to_owned(),
            "ThinkingDelta(\"thinking\")".to_owned(),
            "TextDelta(\"lo\")".to_owned(),
            "Usage(Usage { input_tokens: 7, output_tokens: 2, cached_input_tokens: 1 })".to_owned(),
        ]
    );
    server.verify().await;

    // The conversation's prompt-cache key rides the body, stable for
    // the provider's lifetime.
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty()),
        "{body}"
    );
}

#[tokio::test]
async fn a_bare_access_token_is_accepted_and_sends_no_account_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer raw-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-5.1-codex"),
    };
    let mut sink = Collector(vec![]);
    let output = ChatGptProvider::new("raw-access")
        .with_api_base(server.uri())
        .complete(request, &mut sink)
        .await
        .expect("the turn completes");
    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    server.verify().await;
}

#[tokio::test]
async fn an_expired_token_refreshes_before_the_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"access_token":"fresh","refresh_token":"fresh-refresh","expires_in":3600}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer fresh"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-5.1-codex"),
    };
    let mut sink = Collector(vec![]);
    provider(&server.uri(), credential("expired", Some("refresh"), 0))
        .complete(request, &mut sink)
        .await
        .expect("the refreshed turn completes");
    server.verify().await;
}

#[tokio::test]
async fn a_401_refreshes_and_retries_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"access_token":"fresh","expires_in":3600}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer stale"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"expired"}}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer fresh"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-5.1-codex"),
    };
    let mut sink = Collector(vec![]);
    provider(
        &server.uri(),
        credential("stale", Some("refresh"), 4_102_444_800),
    )
    .complete(request, &mut sink)
    .await
    .expect("the retry lands");
    server.verify().await;
}

#[tokio::test]
async fn a_non_2xx_response_keeps_its_status_and_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(r#"{"error":{"message":"usage limit reached"}}"#),
        )
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-5.1-codex"),
    };
    let mut sink = Collector(vec![]);
    let err = provider(&server.uri(), credential("access", None, 4_102_444_800))
        .complete(request, &mut sink)
        .await
        .expect_err("the server rejects the turn");
    assert_eq!(
        err,
        ProviderError::Api {
            message: "chatgpt api error (HTTP 429): usage limit reached".into(),
            status: Some(429),
        }
    );
}

#[tokio::test]
async fn the_models_listing_reads_the_visible_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", MODELS_CLIENT_VERSION))
        .and(header("Authorization", "Bearer access"))
        .and(header("chatgpt-account-id", "acct-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"models":[
                {"slug":"gpt-5.1-codex","display_name":"GPT-5.1 Codex",
                 "description":"Codex","context_window":400000,
                 "input_modalities":["text","image"],"visibility":"list",
                 "supported_in_api":true}
            ]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let models = provider(&server.uri(), credential("access", None, 4_102_444_800))
        .list_models()
        .await
        .expect("the listing lands");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gpt-5.1-codex");
    assert_eq!(models[0].input_token_limit, Some(400_000));
    server.verify().await;
}

#[test]
fn the_defaults_name_the_codex_backend() {
    assert_eq!(DEFAULT_API_BASE, "https://chatgpt.com/backend-api/codex");
    assert_eq!(MODELS_CLIENT_VERSION, "0.167.0");
}

/// Records events as their debug strings; `Event` borrows, so this
/// keeps owned text.
struct Collector(Vec<String>);

impl rhema_core::event::EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}
