//! Offline transport tests against a wiremock server standing in for
//! both GitHub (device flow, token exchange) and the Copilot proxy
//! (chat, models).

use rhema_core::event::Event;
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::StopReason;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ChatParams;
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate};
use rhema_provider_copilot::{CopilotLogin, CopilotProvider};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The exchange endpoint's answer: a short-lived token serving `api`.
fn token_body(api: &str, expires_at: u64) -> String {
    serde_json::json!({
        "token": "tid=abc;exp=1;ctu=1;",
        "expires_at": expires_at,
        "refresh_in": 1500,
        "endpoints": { "api": api }
    })
    .to_string()
}

/// Mounts the exchange endpoint, answering `times` times.
async fn mount_exchange(server: &MockServer, api: &str, expires_at: u64, times: usize) {
    Mock::given(method("GET"))
        .and(path("/copilot_internal/v2/token"))
        .and(header("Authorization", "token gho_test"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(token_body(api, expires_at)),
        )
        .expect(times as u64)
        .mount(server)
        .await;
}

/// A provider aimed at the mock server, with the proxy base overridden
/// so exchange and chat can share one mock (the exchange's
/// `endpoints.api` wins over `with_api_base`, and it points back at the
/// server itself).
fn provider(server_uri: &str) -> CopilotProvider {
    CopilotProvider::new("gho_test")
        .with_github_api_url(server_uri)
        .with_api_base(server_uri)
}

fn sse_body() -> String {
    // One event per line, blank-line separated — consecutive `data:`
    // lines without the separator are one event with joined data.
    [
        r#"data: {"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
        "",
        r#"data: {"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        "",
        r#"data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

#[tokio::test]
async fn chat_completions_streams_through_the_sse_decoder() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer tid=abc;exp=1;ctu=1;"))
        .and(header("Copilot-Integration-Id", "vscode-chat"))
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
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let output = provider(&server.uri())
        .complete(request, &mut sink)
        .await
        .expect("the stream completes");

    assert_eq!(output.stop_reason, StopReason::EndTurn);
    assert_eq!(output.usage.input_tokens, 7);
    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    // Usage rides the final chunk: the shared assembler emits it.
    assert_eq!(
        sink.0,
        vec![
            "TextDelta(\"Hel\")".to_owned(),
            "TextDelta(\"lo\")".to_owned(),
            "Usage(Usage { input_tokens: 7, output_tokens: 2, cached_input_tokens: 0 })".to_owned(),
        ]
    );
}

#[tokio::test]
async fn the_copilot_token_is_exchanged_once_and_cached() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let _ = p.complete(request.clone(), &mut sink).await;
    let _ = p.complete(request.clone(), &mut sink).await;
    let _ = p.complete(request, &mut sink).await;

    // Three turns, one exchange: the cached token served the rest.
    server.verify().await;
}

#[tokio::test]
async fn an_expired_cached_token_is_refreshed() {
    let server = MockServer::start().await;
    // Already expired: every call must re-exchange.
    mount_exchange(&server, &server.uri(), 0, 2).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let _ = p.complete(request.clone(), &mut sink).await;
    let _ = p.complete(request, &mut sink).await;

    server.verify().await;
}

#[tokio::test]
async fn a_401_refreshes_and_retries_once() {
    let server = MockServer::start().await;
    // The initial exchange plus the forced refresh.
    mount_exchange(&server, &server.uri(), 4_102_444_800, 2).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"expired"}}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let output = provider(&server.uri())
        .complete(request, &mut sink)
        .await
        .expect("the retry lands");

    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    server.verify().await;
}

#[tokio::test]
async fn a_second_401_is_the_servers_answer() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 2).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"expired"}}"#),
        )
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let err = provider(&server.uri())
        .complete(request, &mut sink)
        .await
        .expect_err("both attempts fail");
    assert_eq!(
        err,
        ProviderError::Api {
            message: "copilot api error (HTTP 401): expired".into(),
            status: Some(401),
        }
    );
}

#[tokio::test]
async fn a_failed_exchange_names_the_error() {
    let server = MockServer::start().await;
    // The GitHub token is not a Copilot seat.
    Mock::given(method("GET"))
        .and(path("/copilot_internal/v2/token"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"error":"bad_credentials","error_description":"Not Found"}"#),
        )
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    let mut sink = Collector(vec![]);
    let err = provider(&server.uri())
        .complete(request, &mut sink)
        .await
        .expect_err("the exchange fails");
    assert!(matches!(err, ProviderError::Api { .. }), "{err:?}");
}

#[tokio::test]
async fn list_models_keeps_the_chat_rows() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("Authorization", "Bearer tid=abc;exp=1;ctu=1;"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"data":[
                        {"id":"gpt-4.1","name":"GPT-4.1","capabilities":{"type":"chat",
                         "limits":{"max_context_window_tokens":128000,"max_output_tokens":16384}}},
                        {"id":"text-embedding-3-small","capabilities":{"type":"embeddings"}}]}"#,
                ),
        )
        .mount(&server)
        .await;

    let models = provider(&server.uri())
        .list_models()
        .await
        .expect("the listing lands");

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gpt-4.1");
    assert_eq!(models[0].input_token_limit, Some(128_000));
    assert_eq!(models[0].output_token_limit, Some(16_384));
    assert_eq!(models[0].pricing, None);
}

/// Records events as their debug strings; `Event` borrows, so this
/// keeps the assertions format-based.
struct Collector(Vec<String>);

impl rhema_core::event::EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

/// Records the updates a flow reports.
struct Recording(Vec<LoginUpdate>);

impl LoginSink for Recording {
    fn update(&mut self, update: LoginUpdate) {
        self.0.push(update);
    }
}

/// A login flow aimed at the mock server, polling without pauses.
fn login(server_uri: &str) -> CopilotLogin {
    CopilotLogin::new()
        .with_github_url(server_uri)
        .with_poll_interval(std::time::Duration::from_millis(1))
}

fn device_body() -> String {
    serde_json::json!({
        "device_code": "device_123",
        "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 5
    })
    .to_string()
}

#[tokio::test]
async fn the_device_flow_completes_after_one_pending_poll() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .and(body_partial_json(serde_json::json!({
            "client_id": "Iv1.b507a08c87ecfe98",
            "scope": "read:user"
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(device_body()),
        )
        .expect(1)
        .mount(&server)
        .await;
    // First poll pending, second poll grants.
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_partial_json(serde_json::json!({
            "device_code": "device_123",
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"error":"authorization_pending"}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"access_token":"gho_granted"}"#),
        )
        .mount(&server)
        .await;

    let mut sink = Recording(vec![]);
    login(&server.uri()).run(&mut sink).await;

    assert_eq!(
        sink.0,
        vec![
            LoginUpdate::Instruct(rhema_provider::LoginInstruction {
                text: "open https://github.com/login/device in a browser and enter the code to authorize rhema".to_owned(),
                url: Some("https://github.com/login/device".to_owned()),
                code: Some("ABCD-1234".to_owned()),
            }),
            LoginUpdate::Waiting("waiting for you to finish in the browser".to_owned()),
            LoginUpdate::Completed("gho_granted".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_denied_authorization_fails_the_flow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(device_body()),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"error":"access_denied"}"#),
        )
        .mount(&server)
        .await;

    let mut sink = Recording(vec![]);
    login(&server.uri()).run(&mut sink).await;

    assert_eq!(
        sink.0,
        vec![
            sink.0[0].clone(),
            LoginUpdate::Failed("authorization was denied on GitHub".to_owned()),
        ]
    );
}

#[tokio::test]
async fn an_expired_device_code_fails_the_flow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(device_body()),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"error":"expired_token"}"#),
        )
        .mount(&server)
        .await;

    let mut sink = Recording(vec![]);
    login(&server.uri()).run(&mut sink).await;

    assert_eq!(
        sink.0[1],
        LoginUpdate::Failed("the device code expired before authorization".to_owned())
    );
}

#[tokio::test]
async fn a_failed_registration_fails_without_instructions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let mut sink = Recording(vec![]);
    login(&server.uri()).run(&mut sink).await;

    assert!(matches!(sink.0[0], LoginUpdate::Failed(_)), "{:?}", sink.0);
}
