//! Offline integration tests: the provider against an in-process wiremock
//! server speaking SSE. Zero network.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, ToolSpec, Usage,
};
use rhema_provider_openrouter::OpenRouterProvider;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Collector(Vec<String>);

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

fn provider(server: &MockServer) -> OpenRouterProvider {
    OpenRouterProvider::new("secret-key").with_base_url(server.uri())
}

fn request(messages: Vec<Message>, tools: Vec<ToolSpec>) -> ModelRequest {
    ModelRequest {
        system: None,
        messages,
        tools,
        params: ChatParams::new("openai/gpt-4o-mini"),
    }
}

/// OpenRouter's stream shape: keep-alive comments, a content-bearing chunk
/// repeating the finish reason, and the usage chunk carrying a content-free
/// choice (its documented deviation from OpenAI's empty-choices usage
/// chunk) before [DONE].
fn sse_body() -> String {
    [
        ": OPENROUTER PROCESSING\n\n",
        "data: {\"id\":\"gen-1\",\"provider\":\"openai\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He\",\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\",\"role\":\"assistant\"},\"finish_reason\":\"stop\",\"native_finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"role\":\"assistant\"},\"finish_reason\":\"stop\",\"native_finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4,\"total_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":2},\"cost\":0.000012}}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat()
}

#[tokio::test]
async fn streams_sse_into_events_and_an_assembled_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect("streaming call succeeds");

    assert_eq!(
        out.message.content,
        vec![Content::Text("Hello".into())],
        "chunk boundaries stay out of the assembled message"
    );
    assert_eq!(out.stop_reason, StopReason::EndTurn);
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 4,
            cached_input_tokens: 2,
        }
    );
    // The empty delta of the usage chunk emits nothing; keep-alives are
    // ignored.
    assert_eq!(sink.0, vec!["TextDelta(\"He\")", "TextDelta(\"llo\")"]);

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], "openai/gpt-4o-mini");
    assert_eq!(body["stream"], json!(true));
    // Usage is unconditional on OpenRouter; the deprecated opt-in is not
    // sent.
    assert!(body.get("stream_options").is_none());
    assert_eq!(
        body["messages"][0],
        json!({ "role": "user", "content": "hi" })
    );
    // The attribution pair OpenRouter's docs recommend rides on every
    // request.
    let headers = &received[0].headers;
    assert_eq!(
        headers.get("x-title").and_then(|value| value.to_str().ok()),
        Some("rhema")
    );
    assert_eq!(
        headers
            .get("http-referer")
            .and_then(|value| value.to_str().ok()),
        Some("https://github.com/vhaudiquet/rhema")
    );
}

#[tokio::test]
async fn tool_call_fragments_stream_into_events_and_a_tool_use_output() {
    let server = MockServer::start().await;
    let body = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"echo\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n",
        "data: [DONE]\n\n",
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
            request(vec![Message::text(Role::User, "echo 1")], vec![]),
            &mut sink,
        )
        .await
        .expect("streaming call succeeds");

    assert_eq!(
        out.message.content,
        vec![Content::ToolCall(ToolCall {
            id: "call_abc".into(),
            name: "echo".into(),
            args: json!({ "x": 1 }),
            provider_data: serde_json::Value::Null,
        })]
    );
    assert_eq!(out.stop_reason, StopReason::ToolUse);
    assert_eq!(out.usage.output_tokens, 3);
    assert_eq!(
        sink.0,
        vec![
            "ToolCallStart { id: \"call_abc\", name: \"echo\" }".to_owned(),
            "ToolCallArgsDelta(\"{\\\"x\\\":\")".to_owned(),
            "ToolCallArgsDelta(\"1}\")".to_owned(),
        ]
    );
}

#[tokio::test]
async fn history_with_tool_calls_maps_to_the_openai_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let out = provider(&server)
        .complete(
            request(
                vec![
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
                        content: vec![Content::ToolResult(rhema_core::ToolResult::success(
                            "call_0",
                            "[\"src\"]",
                        ))],
                    },
                ],
                vec![ToolSpec::new(
                    "list_dir",
                    "lists a directory",
                    json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
                )],
            ),
            &mut sink,
        )
        .await
        .expect("the call succeeds even with an empty stream");

    // The [DONE]-only stream assembles an empty assistant message.
    assert_eq!(out.message.content, Vec::<Content>::new());

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "checking");
    assert_eq!(
        messages[1]["tool_calls"][0],
        json!({
            "id": "call_0",
            "type": "function",
            "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" }
        })
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_0");
    assert_eq!(messages[2]["content"], "[\"src\"]");
    assert_eq!(
        body["tools"][0],
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "lists a directory",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }
            }
        })
    );
}

#[tokio::test]
async fn the_api_key_rides_only_in_the_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer secret-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    provider(&server)
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect("the bearer-authenticated call succeeds");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    // The key is a header, never part of the URL.
    assert!(!server.uri().contains("secret-key"));
    assert_eq!(
        received[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret-key")
    );
}

#[tokio::test]
async fn an_empty_api_key_omits_the_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;

    // The catalog is public: listing works with no key at all. Completions
    // would answer 401 the same way.
    let models = OpenRouterProvider::new("")
        .with_base_url(server.uri())
        .list_models()
        .await
        .expect("the anonymous models call succeeds");
    assert!(models.is_empty());

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert!(received[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                json!({
                    "error": {
                        "code": 401,
                        "message": "No auth credentials found",
                        "metadata": { "host": "openrouter.ai" }
                    }
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let err = provider(&server)
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect_err("non-2xx is an error");

    match err {
        ProviderError::Api(message) => {
            assert!(message.contains("401"), "got: {message}");
            assert!(
                message.contains("No auth credentials found"),
                "got: {message}"
            );
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_mid_stream_error_chunk_maps_to_an_api_error() {
    let server = MockServer::start().await;
    // The documented post-commit failure: HTTP 200, top-level error with a
    // string code, finish_reason "error", then the stream ends.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"so f\"}}]}\n\n\
                data: {\"id\":\"cmpl-1\",\"error\":{\"code\":\"server_error\",\"message\":\"Provider disconnected unexpectedly\"},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n\n";
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
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect_err("a mid-stream error is an error");
    assert_eq!(
        err,
        ProviderError::Api("Provider disconnected unexpectedly".into())
    );
}

#[tokio::test]
async fn a_transport_failure_maps_to_other() {
    // An unroutable address: nothing is listening, so the connect fails.
    let server = OpenRouterProvider::new("k").with_base_url("http://127.0.0.1:1");
    let mut sink = Collector(vec![]);
    let err = server
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ProviderError::Other(_)), "got: {err:?}");
}

#[tokio::test]
async fn list_models_maps_entries_to_gemini_rows_with_per_mtok_pricing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "openai/gpt-4o-mini",
                    "canonical_slug": "openai/gpt-4o-mini-2024-07-18",
                    "name": "GPT-4o mini",
                    "created": 1721172741,
                    "description": "the small openai model",
                    "context_length": 128000,
                    "architecture": {
                        "modality": "text->text",
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"],
                        "tokenizer": "GPT"
                    },
                    "pricing": {
                        "prompt": "0.0000015",
                        "completion": "0.000006",
                        "request": "0",
                        "image": "0.003613"
                    },
                    "top_provider": {
                        "context_length": 128000,
                        "is_moderated": true,
                        "max_completion_tokens": 16384
                    },
                    "supported_parameters": ["tools", "temperature", "max_tokens"]
                },
                {
                    "id": "some/unpriced-model",
                    "name": "Unpriced"
                }
            ],
            "total_count": 2,
            "links": { "next": null }
        })))
        .mount(&server)
        .await;

    let models = provider(&server).list_models().await.expect("models parse");

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "openai/gpt-4o-mini");
    assert_eq!(models[0].display_name, "GPT-4o mini");
    assert_eq!(models[0].input_token_limit, Some(128_000));
    assert_eq!(models[0].output_token_limit, None);
    assert_eq!(models[0].generation_methods, vec!["generateContent"]);
    // Per-token strings scale by 1M: 0.0000015 USD/token = 1.5 USD/MTok.
    let pricing = models[0].pricing.expect("the entry is priced");
    assert_eq!(pricing.input_per_mtok, 1.5);
    assert_eq!(pricing.output_per_mtok, 6.0);
    // model.pricing() prefers the instance price; the catalog id is no
    // Gemini, so the curated table has no fallback here.
    assert_eq!(models[0].pricing(), models[0].pricing);
    assert_eq!(models[1].pricing, None);

    // The catalog call carries the attribution headers too, and no key was
    // required to have matched above.
    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(
        received[0]
            .headers
            .get("x-title")
            .and_then(|value| value.to_str().ok()),
        Some("rhema")
    );
}

#[tokio::test]
async fn list_models_follows_pagination() {
    let server = MockServer::start().await;
    // Mount order matters: the offset request must match before the plain
    // one, exactly like the Gemini adapter's page-token test.
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "b/c",
                    "name": "C",
                    "pricing": { "prompt": "0.000001", "completion": "0.000002" }
                }
            ],
            "links": { "next": null }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "id": "a/b", "name": "B" },
                { "id": "x/y", "name": "Y" }
            ],
            "links": { "next": "/api/v1/models?limit=1000&offset=2" }
        })))
        .mount(&server)
        .await;

    let models = provider(&server)
        .list_models()
        .await
        .expect("pages assemble");
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["a/b", "x/y", "b/c"]);

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 2);
}

#[tokio::test]
async fn trailing_slashes_in_the_base_url_are_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "a/b", "name": "B" }]
        })))
        .mount(&server)
        .await;

    let models = OpenRouterProvider::new("")
        .with_base_url(format!("{}/", server.uri()))
        .list_models()
        .await
        .expect("models parse");
    assert_eq!(models.len(), 1);
}
