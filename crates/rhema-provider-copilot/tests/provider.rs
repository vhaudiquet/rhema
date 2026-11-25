//! Offline transport tests against a wiremock server standing in for
//! both GitHub (device flow, token exchange) and the Copilot proxy
//! (chat, models).

use rhema_core::event::Event;
use rhema_core::message::{Content, Message, Role};
use rhema_core::output::StopReason;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ChatParams;
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate, ModelListing};
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
    // One event per line, blank-line separated, consecutive `data:`
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

/// An Anthropic Messages stream: header, one text block, the final
/// stop with its token counts, the closing frame.
fn messages_sse_body() -> String {
    [
        "event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":9,"output_tokens":1}}}"#,
        "",
        "event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        "event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        "",
        "event: content_block_delta",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
        "",
        "event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        "event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        "",
        "event: message_stop",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

#[tokio::test]
async fn a_claude_model_runs_the_messages_route_with_its_breakpoints() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        // The Messages route authenticates with the current Copilot
        // token as the bearer, same as the chat route, and carries the
        // editor identity.
        .and(header("Authorization", "Bearer tid=abc;exp=1;ctu=1;"))
        .and(header("Copilot-Integration-Id", "vscode-chat"))
        // The caching opt-in: the native Anthropic wire marks the
        // system prompt, and with tools the last tool and the
        // conversation tail. The system breakpoint alone proves the
        // route builds the Messages body, not a chat-completions one.
        .and(body_partial_json(serde_json::json!({
            "model": "claude-sonnet-4-5",
            "system": [{"cache_control": {"type": "ephemeral"}}]
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(messages_sse_body()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = rhema_core::ModelRequest {
        system: Some("be terse".into()),
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("claude-sonnet-4-5"),
    };
    let mut sink = Collector(vec![]);
    let output = provider(&server.uri())
        .complete(request, &mut sink)
        .await
        .expect("the stream completes");

    assert_eq!(output.stop_reason, StopReason::EndTurn);
    // Cache reads surface through the shared usage shape.
    assert_eq!(output.usage.input_tokens, 9);
    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    // The Messages assembler emits message_start's usage first, the
    // deltas, then the final chunk's usage, the shape the native
    // Anthropic adapter streams.
    assert_eq!(
        sink.0,
        vec![
            "Usage(Usage { input_tokens: 9, output_tokens: 1, cached_input_tokens: 0 })".to_owned(),
            "TextDelta(\"Hel\")".to_owned(),
            "TextDelta(\"lo\")".to_owned(),
            "Usage(Usage { input_tokens: 9, output_tokens: 4, cached_input_tokens: 0 })".to_owned(),
        ]
    );
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

/// A Responses-API stream: two text deltas, then the terminal event
/// with its usage, the shape the shared drain expects.
fn responses_sse() -> String {
    [
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n",
    ]
    .concat()
}

#[tokio::test]
async fn a_model_the_chat_endpoint_disowns_moves_to_responses_and_stays_there() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    // The chat endpoint names the route once; the turn moves, and the
    // next turn goes straight there without asking chat again.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({ "model": "gpt-6-luna" })))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"model \"gpt-6-luna\" is not accessible via the /chat/completions endpoint"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("Authorization", "Bearer tid=abc;exp=1;ctu=1;"))
        .and(header("Copilot-Integration-Id", "vscode-chat"))
        .and(body_partial_json(
            serde_json::json!({ "model": "gpt-6-luna" }),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_sse()),
        )
        .expect(2)
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("gpt-6-luna"),
    };
    let mut sink = Collector(vec![]);
    for turn in 0..2 {
        let output = p
            .complete(request.clone(), &mut sink)
            .await
            .unwrap_or_else(|err| panic!("turn {turn}: {err:?}"));
        assert_eq!(output.stop_reason, StopReason::EndTurn);
        assert_eq!(output.usage.input_tokens, 7);
        assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    }
    server.verify().await;
}

#[tokio::test]
async fn the_models_page_teaches_the_route_before_the_first_turn() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"data":[
                        {"id":"gpt-6-luna","name":"GPT-6 Luna","supported_endpoints":["/responses"],
                         "capabilities":{"type":"chat","limits":{"max_context_window_tokens":400000,"max_output_tokens":128000}}},
                        {"id":"gpt-4.1","name":"GPT-4.1","supported_endpoints":["/chat/completions","/responses"],
                         "capabilities":{"type":"chat"}}]}"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_sse()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let models = p.list_models().await.expect("the listing lands");
    assert_eq!(models.len(), 2);

    let request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("gpt-6-luna"),
    };
    let mut sink = Collector(vec![]);
    let output = p
        .complete(request, &mut sink)
        .await
        .expect("the learned route serves the turn");
    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);
    server.verify().await;
}

#[tokio::test]
async fn a_chat_400_that_is_not_a_route_hint_stays_the_servers_answer() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(r#"{"error":{"message":"bad request"}}"#),
        )
        .expect(1)
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
        .expect_err("the server refuses, and no route is named");
    assert_eq!(
        err,
        ProviderError::Api {
            message: "copilot api error (HTTP 400): bad request".into(),
            status: Some(400),
        }
    );
    server.verify().await;
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

#[tokio::test]
async fn a_chat_400_naming_reasoning_effort_retries_without_it() {
    let server = MockServer::start().await;
    mount_exchange(&server, &server.uri(), 4_102_444_800, 1).await;
    // A model without a reasoning level: the proxy refuses the field the
    // app sends by default, and the plain retry is the turn.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({ "reasoning_effort": "medium" })))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported parameter: 'reasoning_effort' is not supported with this model."}}"#,
        ))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .with_priority(2)
        .expect(2)
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let mut request = rhema_core::ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "say hello")],
        tools: vec![],
        params: ChatParams::new("gpt-4.1"),
    };
    request.params.thinking = rhema_core::request::ThinkingEffort::Medium;
    let repeat = request.clone();
    let mut sink = Collector(vec![]);
    let output = p
        .complete(request, &mut sink)
        .await
        .expect("the plain retry lands");
    assert_eq!(output.message.content, vec![Content::Text("Hello".into())]);

    // The refusal is remembered: the next turn drops the field before
    // sending, with no second 400.
    p.complete(repeat, &mut sink)
        .await
        .expect("the next turn goes out plain");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    let chat: Vec<_> = received
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect();
    assert_eq!(chat.len(), 3);
    let first: serde_json::Value = serde_json::from_slice(&chat[0].body).expect("json body");
    assert_eq!(first["reasoning_effort"], "medium");
    for body in &chat[1..] {
        let body: serde_json::Value = serde_json::from_slice(&body.body).expect("json body");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }
    server.verify().await;
}
