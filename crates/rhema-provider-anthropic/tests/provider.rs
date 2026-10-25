//! Offline integration tests: the provider against an in-process wiremock
//! server speaking SSE. Zero network.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, ToolResult, ToolSpec, Usage,
};
use rhema_provider_anthropic::AnthropicProvider;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Collector(Vec<String>);

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

fn provider(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::new("test-key").with_base_url(server.uri())
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        system: None,
        messages,
        tools: vec![],
        params: ChatParams::new("claude-sonnet-4-5"),
    }
}

fn event(kind: &str, payload: &str) -> String {
    format!("event: {kind}\ndata: {payload}\n\n")
}

/// A plain-text Messages stream: header, one text block, the final stop
/// reason with its output-token count, and the closing frame.
fn sse_body() -> String {
    [
        event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":9,"output_tokens":1}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"He"}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"llo"}}"#,
        ),
        event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
    .concat()
}

#[tokio::test]
async fn streams_sse_into_events_and_an_assembled_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect("streaming call succeeds");

    // One streamed text block: its deltas accumulate into one content
    // block of the assembled message.
    assert_eq!(out.message.content, vec![Content::Text("Hello".into())]);
    assert_eq!(out.stop_reason, StopReason::EndTurn);
    // input from message_start, output from message_delta.
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 4,
            cached_input_tokens: 0,
        }
    );
    // The tally streams live — the prompt from message_start, the
    // completion from message_delta; stops and the closing frame emit
    // nothing.
    assert_eq!(
        sink.0,
        vec![
            "Usage(Usage { input_tokens: 9, output_tokens: 1, cached_input_tokens: 0 })",
            "TextDelta(\"He\")",
            "TextDelta(\"llo\")",
            "Usage(Usage { input_tokens: 9, output_tokens: 4, cached_input_tokens: 0 })",
        ]
    );

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], "claude-sonnet-4-5");
    // The API-required ceiling defaults when ChatParams leaves it unset.
    assert_eq!(
        body["max_tokens"],
        json!(rhema_provider_anthropic::DEFAULT_MAX_TOKENS)
    );
    assert_eq!(body["stream"], json!(true));
    assert!(body.get("system").is_none());
    assert_eq!(
        body["messages"][0],
        json!({ "role": "user", "content": [{ "type": "text", "text": "hi",
                 "cache_control": { "type": "ephemeral" } }] })
    );
    // The key is a header, never part of the URL.
    assert!(!server.uri().contains("test-key"));
}

#[tokio::test]
async fn a_bearer_provider_authenticates_by_the_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event("message_stop", r#"{"type":"message_stop"}"#)),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    provider(&server)
        .with_bearer_auth()
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect("the bearer-authenticated call succeeds");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    // The bearer scheme replaces x-api-key, it does not join it.
    assert!(received[0].headers.get("x-api-key").is_none());
    // Every request identifies its client honestly.
    let agent = received[0]
        .headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(agent.starts_with("rhema/"), "got user-agent: {agent}");
}

#[tokio::test]
async fn a_tool_use_reply_streams_call_events_and_multi_chunk_args() {
    let server = MockServer::start().await;
    let body = [
        event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"running"}}"#,
        ),
        event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01A","name":"echo","input":{}}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
        ),
        event("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
        event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        ),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
    .concat();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(
            request(vec![Message::text(Role::User, "echo 1")]),
            &mut sink,
        )
        .await
        .expect("streaming call succeeds");

    assert_eq!(
        out.message.content,
        vec![
            Content::Text("running".into()),
            Content::ToolCall(ToolCall {
                id: "toolu_01A".into(),
                name: "echo".into(),
                args: json!({ "x": 1 }),
                provider_data: serde_json::Value::Null,
            }),
        ]
    );
    assert_eq!(out.stop_reason, StopReason::ToolUse);
    assert_eq!(out.usage.output_tokens, 3);
    assert_eq!(
        sink.0,
        vec![
            "Usage(Usage { input_tokens: 5, output_tokens: 1, cached_input_tokens: 0 })".to_owned(),
            "TextDelta(\"running\")".to_owned(),
            "ToolCallStart { id: \"toolu_01A\", name: \"echo\" }".to_owned(),
            "ToolCallArgsDelta { id: \"toolu_01A\", delta: \"{\\\"x\\\":\" }".to_owned(),
            "ToolCallArgsDelta { id: \"toolu_01A\", delta: \"1}\" }".to_owned(),
            "Usage(Usage { input_tokens: 5, output_tokens: 3, cached_input_tokens: 0 })".to_owned(),
        ]
    );
}

#[tokio::test]
async fn history_with_tool_results_maps_to_the_messages_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(event("message_stop", r#"{"type":"message_stop"}"#)),
        )
        .mount(&server)
        .await;

    let request = ModelRequest {
        system: Some("be terse".into()),
        messages: vec![
            Message::text(Role::User, "list files"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("checking".into()),
                    Content::ToolCall(ToolCall {
                        id: "call_0".into(),
                        name: "list_dir".into(),
                        args: json!({ "path": "." }),
                        provider_data: serde_json::Value::Null,
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::success(
                    "call_0",
                    "[\"src\"]",
                ))],
            },
            Message::text(Role::User, "thanks"),
        ],
        tools: vec![ToolSpec::new(
            "list_dir",
            "lists a directory",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )],
        params: ChatParams {
            model: "claude-sonnet-4-5".into(),
            temperature: Some(0.2),
            max_tokens: Some(256),
            thinking: rhema_core::request::ThinkingEffort::Default,
        },
    };

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(request, &mut sink)
        .await
        .expect("the call succeeds even with an empty stream");

    // The message_stop-only stream assembles an empty assistant message.
    assert_eq!(out.message.content, Vec::<Content>::new());

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        body["system"],
        json!([{ "type": "text", "text": "be terse",
                 "cache_control": { "type": "ephemeral" } }])
    );
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["temperature"], json!(0.2));
    assert_eq!(
        body["tools"][0],
        json!({
            "name": "list_dir",
            "description": "lists a directory",
            "input_schema": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            },
            "cache_control": { "type": "ephemeral" }
        })
    );
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[1]["content"],
        json!([
            { "type": "text", "text": "checking" },
            { "type": "tool_use", "id": "call_0", "name": "list_dir", "input": { "path": "." } }
        ])
    );
    // The tool result rides in a user message, grouped with the following
    // user turn, results first.
    assert_eq!(
        messages[2],
        json!({
            "role": "user",
            "content": [
                { "type": "tool_result", "tool_use_id": "call_0", "content": "[\"src\"]", "is_error": false },
                { "type": "text", "text": "thanks",
                  "cache_control": { "type": "ephemeral" } }
            ]
        })
    );
}

#[tokio::test]
async fn pings_in_the_stream_are_ignored() {
    let server = MockServer::start().await;
    let body = [
        event("ping", r#"{"type":"ping"}"#),
        event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_3","type":"message","role":"assistant","usage":{"input_tokens":1}}}"#,
        ),
        event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ),
        event("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        event("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#),
        event("message_stop", r#"{"type":"message_stop"}"#),
    ]
    .concat();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect("pings do not break the stream");

    assert_eq!(out.message.content, vec![Content::Text("ok".into())]);
    assert_eq!(out.stop_reason, StopReason::EndTurn);
    assert_eq!(
        sink.0,
        vec![
            "Usage(Usage { input_tokens: 1, output_tokens: 0, cached_input_tokens: 0 })".to_owned(),
            "TextDelta(\"ok\")".to_owned(),
        ]
    );
}

#[tokio::test]
async fn an_sse_error_event_maps_to_an_api_error() {
    let server = MockServer::start().await;
    // The documented post-commit failure: HTTP 200, then an error event.
    let body = [
        event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"so f"}}"#,
        ),
        event(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ),
    ]
    .concat();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let err = provider(&server)
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect_err("a mid-stream error event is an error");
    assert_eq!(
        err,
        ProviderError::Api {
            message: "Overloaded".into(),
            status: None,
        }
    );
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                json!({
                    "type": "error",
                    "error": {
                        "type": "authentication_error",
                        "message": "invalid x-api-key"
                    }
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let err = provider(&server)
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect_err("non-2xx is an error");

    match err {
        ProviderError::Api { message, status } => {
            assert_eq!(status, Some(401));
            assert!(message.contains("401"), "got: {message}");
            assert!(message.contains("invalid x-api-key"), "got: {message}");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_rate_limit_names_its_reset_time_and_does_not_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "37")
                .set_body_string(
                    json!({
                        "type": "error",
                        "error": {
                            "type": "rate_limit_error",
                            "message": "usage limit reached"
                        }
                    })
                    .to_string(),
                ),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let err = provider(&server)
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect_err("a rate limit is an error");

    match &err {
        ProviderError::Api { message, status } => {
            assert_eq!(*status, Some(429));
            assert!(message.contains("429"), "got: {message}");
            assert!(message.contains("usage limit reached"), "got: {message}");
            assert!(
                message.contains("retry-after: 37"),
                "the reset time rides the message: {message}"
            );
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
    // The wall does not retry above this seam either: the ladder cannot
    // clear a quota window.
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_transport_failure_maps_to_other() {
    // An unroutable address: nothing is listening, so the connect fails.
    let provider = AnthropicProvider::new("k").with_base_url("http://127.0.0.1:1");
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(request(vec![Message::text(Role::User, "hi")]), &mut sink)
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ProviderError::Network(_)), "got: {err:?}");
}

#[tokio::test]
async fn invalid_requests_fail_before_any_http() {
    // An unroutable address: only pre-wire validation can pass here.
    let provider = AnthropicProvider::new("test-key").with_base_url("http://127.0.0.1:1");
    let mut sink = Collector(vec![]);

    let err = provider
        .complete(
            ModelRequest {
                system: None,
                messages: vec![Message::text(Role::User, "hi")],
                tools: vec![],
                params: ChatParams::new(""),
            },
            &mut sink,
        )
        .await
        .expect_err("empty model is invalid");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));

    let err = provider
        .complete(
            ModelRequest {
                system: None,
                messages: vec![],
                tools: vec![],
                params: ChatParams::new("claude-sonnet-4-5"),
            },
            &mut sink,
        )
        .await
        .expect_err("no messages is invalid");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

// The after_id mock mounts first: a request carrying `after_id` matches
// both mounts, and wiremock serves the first match.
#[tokio::test]
async fn list_models_follows_pagination_and_carries_the_auth_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "claude-opus-4-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5" }
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5" },
                { "type": "model", "id": "claude-opus-4-1", "display_name": "Claude Opus 4.1" }
            ],
            "has_more": true,
            "first_id": "claude-sonnet-4-5",
            "last_id": "claude-opus-4-1"
        })))
        .mount(&server)
        .await;

    let models = provider(&server)
        .list_models()
        .await
        .expect("pages assemble");

    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"]
    );
    assert_eq!(models[0].display_name, "Claude Sonnet 4.5");
    assert!(models[0].supports_chat());

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 2);
    // The models call authenticates the same way the messages call does.
    assert_eq!(
        received[0]
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("test-key")
    );
    assert_eq!(
        received[0]
            .headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01")
    );
    // And the honest client mark rides along.
    let agent = received[0]
        .headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(agent.starts_with("rhema/"), "got user-agent: {agent}");
}

#[tokio::test]
async fn trailing_slashes_in_the_base_url_are_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "type": "model", "id": "claude-sonnet-4-5", "display_name": "S" }]
        })))
        .mount(&server)
        .await;

    let models = AnthropicProvider::new("test-key")
        .with_base_url(format!("{}/", server.uri()))
        .list_models()
        .await
        .expect("models parse");
    assert_eq!(models.len(), 1);
}

#[tokio::test]
async fn a_non_zai_base_has_no_allowance_to_report() {
    // The mock server's origin is no Z.ai host: the allowance degrades
    // to none without a call — no mock is mounted, so any request would
    // 404 the result into an error instead.
    let server = MockServer::start().await;
    let allowance = provider(&server).allowance().await;
    assert_eq!(allowance, Ok(None));
}
