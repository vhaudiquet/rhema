//! Offline integration tests: the provider against an in-process wiremock
//! server speaking SSE. Zero network.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, Usage,
};
use rhema_provider_google::GoogleProvider;
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
            "ToolCallArgsDelta(\"{\\\"x\\\":1}\")".to_owned(),
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
        ProviderError::Api(message) => {
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

    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        ["gemini-3.7-flash", "text-embedding-1", "gemini-3.8-flash"]
    );
    assert_eq!(models[0].display_name, "Gemini 3.7 Flash");
    assert_eq!(models[0].input_token_limit, Some(1_000_000));
    assert!(models[0].supports_chat());
    assert!(!models[1].supports_chat());
}
