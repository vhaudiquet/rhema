//! Offline integration tests: the provider against an in-process wiremock
//! server speaking SSE. Zero network.

use std::time::Duration;

use rhema_core::request::ThinkingEffort;
use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, Usage,
};
use rhema_provider::ModelListing;
use rhema_provider_google::{GoogleProvider, model_pricing};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Collector(Vec<String>);

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

fn request(model: &str, messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        system: None,
        messages,
        tools: vec![],
        params: ChatParams::new(model),
    }
}

fn sse_body() -> String {
    [
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"He\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"echo\",\"args\":{\"x\":1}}}]},\"finishReason\":\"STOP\"}]}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":4,\"cachedContentTokenCount\":2}}\n\n",
    ]
    .concat()
}

#[tokio::test]
async fn an_oversized_sse_line_fails_the_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("x".repeat(2 * 1024 * 1024)),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("an oversized line must fail the stream");
    assert!(
        matches!(err, ProviderError::Network(ref message) if message.contains("without a line break")),
        "the decoder error is propagated: {err:?}"
    );
}

#[tokio::test]
async fn streams_sse_into_events_and_an_assembled_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let out = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect("streaming call succeeds");

    // The assembled output mirrors the streamed parts in order, with the
    // synthesized id and the last-wins finish reason and usage.
    assert_eq!(
        out.message.content,
        vec![
            Content::Text("He".into()),
            Content::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "echo".into(),
                args: json!({ "x": 1 }),

                provider_data: serde_json::Value::Null
            }),
        ]
    );
    assert_eq!(out.stop_reason, StopReason::ToolUse);
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 4,
            cached_input_tokens: 2,
        }
    );
    assert_eq!(
        sink.0,
        vec![
            "TextDelta(\"He\")".to_owned(),
            "ToolCallStart { id: \"call_0\", name: \"echo\" }".to_owned(),
            "ToolCallArgsDelta { id: \"call_0\", delta: \"{\\\"x\\\":1}\" }".to_owned(),
            // The final chunk carries the usage: the live tally closes
            // the stream.
            "Usage(Usage { input_tokens: 9, output_tokens: 4, cached_input_tokens: 2 })".to_owned(),
        ]
    );

    // The wire request carries the system prompt and camelCase fields.
    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        body["contents"][0],
        json!({ "role": "user", "parts": [{ "text": "hi" }] })
    );
    assert!(body.get("systemInstruction").is_none());
}

#[tokio::test]
async fn a_stream_cut_before_its_finish_reason_fails_the_turn() {
    // Text streams, then the connection drops: the chunk carries no
    // `finishReason`. The old leniency stood the partial answer up as
    // a clean EndTurn; now the truncation is a retryable failure.
    let server = MockServer::start().await;
    let body = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"par\"}]}}]}\n\n\
                data: {\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1}}\n\n";
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("a stream cut before its finishReason must fail");
    assert!(
        matches!(err, ProviderError::Network(ref message) if message.contains("without its finishReason")),
        "the truncation error is propagated: {err:?}"
    );
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                json!({
                    "error": {
                        "code": 400,
                        "message": "Contents are not specified",
                        "status": "INVALID_ARGUMENT",
                    }
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("non-2xx is an error");

    match err {
        ProviderError::Api { message, status } => {
            assert_eq!(status, Some(400));
            assert!(message.contains("400"), "got: {message}");
            assert!(
                message.contains("Contents are not specified"),
                "got: {message}"
            );
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_429_with_a_retry_info_delay_becomes_a_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                // Both a header and a body detail: the body's own
                // clock names the specific quota's wait, so it wins.
                .insert_header("retry-after", "9")
                .set_body_string(
                    json!({
                        "error": {
                            "code": 429,
                            "message": "Resource has been exhausted (e.g. daily quota).",
                            "status": "RESOURCE_EXHAUSTED",
                            "details": [{
                                "@type": "type.googleapis.com/google.rpc.RetryInfo",
                                "retryDelay": "30s"
                            }]
                        }
                    })
                    .to_string(),
                ),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("a rate limit is an error");

    match &err {
        ProviderError::RateLimited {
            message,
            retry_after,
        } => {
            assert_eq!(*retry_after, Duration::from_secs(30));
            assert!(
                message.contains("Resource has been exhausted"),
                "got: {message}"
            );
        }
        other => panic!("expected RateLimited, got: {other:?}"),
    }
    assert!(err.is_retryable(), "a minute window clears in a retry");
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_429_with_only_the_retry_after_header_becomes_a_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "12")
                .set_body_string(
                    json!({
                        "error": {
                            "code": 429,
                            "message": "Resource has been exhausted (e.g. daily quota).",
                            "status": "RESOURCE_EXHAUSTED"
                        }
                    })
                    .to_string(),
                ),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("a rate limit is an error");

    assert!(
        matches!(
            &err,
            ProviderError::RateLimited { retry_after, .. }
                if *retry_after == Duration::from_secs(12)
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_bare_429_stays_a_plain_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(
                json!({
                    "error": {
                        "code": 429,
                        "message": "Resource has been exhausted (e.g. daily quota).",
                        "status": "RESOURCE_EXHAUSTED"
                    }
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("the server rejects the turn");

    // A 429 that names no reset is a quota, not a short window: the
    // door stays shut, the fleet's deliberate named-only convention.
    assert_eq!(
        err,
        ProviderError::Api {
            message: "gemini api error (HTTP 429): Resource has been exhausted (e.g. daily quota)."
                .into(),
            status: Some(429),
        }
    );
    assert!(!err.is_retryable());
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_models_list_429_with_a_retry_after_becomes_a_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_string(
                    json!({
                        "error": {
                            "code": 429,
                            "message": "Resource has been exhausted (e.g. daily quota).",
                            "status": "RESOURCE_EXHAUSTED"
                        }
                    })
                    .to_string(),
                ),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let err = provider
        .list_models()
        .await
        .expect_err("a rate limit is an error");

    assert!(
        matches!(
            &err,
            ProviderError::RateLimited { retry_after, .. }
                if *retry_after == Duration::from_secs(7)
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_mid_stream_error_chunk_maps_to_an_api_error() {
    let server = MockServer::start().await;
    let body = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"so f\"}]}}]}\n\n\
                data: {\"error\":{\"code\":500,\"message\":\"inference failed\",\"status\":\"UNAVAILABLE\"}}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("a mid-stream error is an error");
    assert_eq!(
        err,
        ProviderError::Api {
            message: "inference failed".into(),
            status: None,
        }
    );
}

#[tokio::test]
async fn invalid_requests_fail_before_any_http() {
    // An unroutable address: only pre-wire validation can pass here.
    let provider = GoogleProvider::new("test-key").with_base_url("http://127.0.0.1:1");
    let mut sink = Collector(vec![]);

    let err = provider
        .complete(
            request("", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect_err("empty model is invalid");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));

    let err = provider
        .complete(request("gemini-2.5-flash", vec![]), &mut sink)
        .await
        .expect_err("no messages is invalid");
    assert!(matches!(err, ProviderError::InvalidRequest(_)));
}

#[tokio::test]
async fn keep_alive_comments_in_the_stream_are_ignored() {
    let server = MockServer::start().await;
    let body = ": ping\n\ndata: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut sink = Collector(vec![]);
    let out = provider
        .complete(
            request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]),
            &mut sink,
        )
        .await
        .expect("keep-alives do not break the stream");

    assert_eq!(out.message.content, vec![Content::Text("ok".into())]);
    assert_eq!(out.stop_reason, StopReason::EndTurn);
    assert_eq!(sink.0, vec!["TextDelta(\"ok\")".to_owned()]);
}

// The page-token mock mounts first: a request carrying `pageToken` matches
// both mounts, and wiremock serves the first match.
#[tokio::test]
async fn list_models_follows_pagination_and_strips_the_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("pageToken", "next-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {
                    "name": "models/gemini-3.8-flash",
                    "displayName": "Gemini 3.8 Flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {
                    "name": "models/gemini-3.7-flash",
                    "displayName": "Gemini 3.7 Flash",
                    "inputTokenLimit": 1000000,
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/text-embedding-1",
                    "supportedGenerationMethods": ["embedContent"]
                }
            ],
            "nextPageToken": "next-1"
        })))
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let models = provider.list_models().await.expect("pages assemble");

    // The embedding row cannot drive a coding agent: filtered. The
    // chat rows keep their page order across the pagination.
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["gemini-3.7-flash", "gemini-3.8-flash"]);
    assert_eq!(models[0].display_name, "Gemini 3.7 Flash");
    assert_eq!(models[0].input_token_limit, Some(1_000_000));
    assert!(models[0].supports_chat());
    // ListModels carries no prices; the curated table is baked in.
    assert_eq!(models[0].pricing, model_pricing("gemini-3.7-flash"));
}

// The zero-row guard: a page that carries a next token but no rows would
// otherwise keep the loop asking forever.
#[tokio::test]
async fn an_empty_page_with_a_next_token_ends_the_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [],
            "nextPageToken": "next-1"
        })))
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let models = provider
        .list_models()
        .await
        .expect("an empty page ends the list");
    assert!(models.is_empty());

    // Exactly one request: the unusable token was not followed.
    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
}

#[tokio::test]
async fn a_rejected_thinking_knob_retries_in_the_other_form() {
    let server = MockServer::start().await;
    // The id rule reads 2.5 as the budget form, but the server says this
    // model wants the named level: the retry follows the server.
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"code":400,"message":"Thinking budget is not supported for this model. Use thinkingLevel instead.","status":"INVALID_ARGUMENT"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut request = request("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]);
    request.params.thinking = ThinkingEffort::High;
    let mut sink = Collector(vec![]);
    provider
        .complete(request, &mut sink)
        .await
        .expect("the retry lands");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 2);
    let first: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        first["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        32_768
    );
    assert!(
        first["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none(),
        "{first}"
    );
    let second: Value = serde_json::from_slice(&received[1].body).expect("json body");
    assert_eq!(
        second["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert!(
        second["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none(),
        "{second}"
    );
}

#[tokio::test]
async fn a_level_rejection_retries_with_the_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-3.8-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"code":400,"message":"Thinking level is not supported for this model.","status":"INVALID_ARGUMENT"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-3.8-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut request = request("gemini-3.8-flash", vec![Message::text(Role::User, "hi")]);
    request.params.thinking = ThinkingEffort::Medium;
    let mut sink = Collector(vec![]);
    provider
        .complete(request, &mut sink)
        .await
        .expect("the retry lands");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 2);
    let first: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        first["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MEDIUM"
    );
    let second: Value = serde_json::from_slice(&received[1].body).expect("json body");
    assert_eq!(
        second["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        16_384
    );
    assert!(
        second["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none(),
        "{second}"
    );
}

#[tokio::test]
async fn an_unrelated_400_is_never_retried_as_a_thinking_knob() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-3.8-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"code":400,"message":"bad request","status":"INVALID_ARGUMENT"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut request = request("gemini-3.8-flash", vec![Message::text(Role::User, "hi")]);
    request.params.thinking = ThinkingEffort::High;
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(request, &mut sink)
        .await
        .expect_err("the server refuses");
    assert!(
        matches!(
            err,
            ProviderError::Api {
                status: Some(400),
                ..
            }
        ),
        "{err:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn a_thinking_refusal_drops_the_config_for_a_model_that_takes_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.0-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"code":400,"message":"Thinking is not supported for this model.","status":"INVALID_ARGUMENT"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/models/gemini-2.0-flash:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = GoogleProvider::new("test-key").with_base_url(server.uri());
    let mut request = request("gemini-2.0-flash", vec![Message::text(Role::User, "hi")]);
    request.params.thinking = ThinkingEffort::Medium;
    let mut sink = Collector(vec![]);
    provider
        .complete(request, &mut sink)
        .await
        .expect("the plain retry lands");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 2);
    let first: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        first["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        16_384
    );
    let second: Value = serde_json::from_slice(&received[1].body).expect("json body");
    assert!(second.get("generationConfig").is_none(), "{second}");
}
