//! Offline integration tests: the provider against an in-process wiremock
//! server speaking SSE. Zero network.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, ToolSpec, Usage,
};
use rhema_provider::{ModelCard, ModelListing};
use rhema_provider_llama_cpp::LlamaCppProvider;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Collector(Vec<String>);

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

fn request(messages: Vec<Message>, tools: Vec<ToolSpec>) -> ModelRequest {
    ModelRequest {
        system: None,
        messages,
        tools,
        params: ChatParams::new("qwen2.5-coder-7b"),
    }
}

fn sse_body() -> String {
    [
        "data: {\"choices\":[{\"delta\":{\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
        ": ping\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4,\"total_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat()
}

#[tokio::test]
async fn streams_sse_into_events_and_an_assembled_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let mut sink = Collector(vec![]);
    let out = provider
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
    // The stream ends at the [DONE] sentinel; keep-alives are ignored.
    // Usage rides the final chunk now: the shared assembler emits it,
    // like every other provider in the family.
    assert_eq!(
        sink.0,
        vec![
            "TextDelta(\"He\")".to_owned(),
            "TextDelta(\"llo\")".to_owned(),
            "Usage(Usage { input_tokens: 9, output_tokens: 4, cached_input_tokens: 2 })".to_owned(),
        ]
    );

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert!(received[0].headers.get("authorization").is_none());
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], "qwen2.5-coder-7b");
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(
        body["messages"][0],
        json!({ "role": "user", "content": "hi" })
    );
}

#[tokio::test]
async fn tool_call_fragments_stream_into_events_and_a_tool_use_output() {
    let server = MockServer::start().await;
    let body = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"echo\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\n",
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

    let provider = LlamaCppProvider::new(server.uri());
    let mut sink = Collector(vec![]);
    let out = provider
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
            "ToolCallArgsDelta { id: \"call_abc\", delta: \"{\\\"x\\\":\" }".to_owned(),
            "ToolCallArgsDelta { id: \"call_abc\", delta: \"1}\" }".to_owned(),
            // Usage rides the final chunk now: the shared assembler
            // emits it, like every other provider in the family.
            "Usage(Usage { input_tokens: 5, output_tokens: 3, cached_input_tokens: 0 })".to_owned(),
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

    let provider = LlamaCppProvider::new(server.uri());
    let mut sink = Collector(vec![]);
    let out = provider
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

    let provider = LlamaCppProvider::new(server.uri()).with_api_key("secret-key");
    let mut sink = Collector(vec![]);
    provider
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
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                json!({
                    "error": {
                        "code": 400,
                        "message": "invalid request: tools must be an array",
                        "type": "invalid_request_error",
                    }
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
            &mut sink,
        )
        .await
        .expect_err("non-2xx is an error");

    match err {
        ProviderError::Api { message, status } => {
            assert_eq!(status, Some(400));
            assert!(message.contains("400"), "got: {message}");
            assert!(message.contains("tools must be an array"), "got: {message}");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
    assert!(sink.0.is_empty());
}

#[tokio::test]
async fn a_mid_stream_error_chunk_maps_to_an_api_error() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"so f\"}}]}\n\n\
                data: {\"error\":{\"code\":500,\"message\":\"inference failed\",\"type\":\"server_error\"}}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let mut sink = Collector(vec![]);
    let err = provider
        .complete(
            request(vec![Message::text(Role::User, "hi")], vec![]),
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

/// The normalized row `list_models` hands out for a model id with the
/// given context window.
fn card(id: &str, context_length: Option<u64>) -> ModelCard {
    ModelCard {
        display_name: id.to_owned(),
        id: id.to_owned(),
        description: String::new(),
        input_token_limit: context_length,
        output_token_limit: None,
        supports_vision: None,
        generation_methods: vec!["generateContent".to_owned()],
        pricing: None,
    }
}

#[tokio::test]
async fn list_models_parses_the_data_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {
                    "id": "qwen2.5-coder-7b",
                    "object": "model",
                    "owned_by": "llamacpp",
                    "meta": { "n_ctx_train": 32768 }
                }
            ]
        })))
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let models = provider.list_models().await.expect("models parse");
    // The meta context comes through the listing; no /props call is needed.
    assert_eq!(models, vec![card("qwen2.5-coder-7b", Some(32768))]);
}

#[tokio::test]
async fn a_single_model_server_gets_its_context_from_props() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "id": "m" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "n_ctx": 8192,
            "default_generation_settings": { "n_ctx": 8192 }
        })))
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let models = provider.list_models().await.expect("models parse");
    assert_eq!(models, vec![card("m", Some(8192))]);
}

#[tokio::test]
async fn router_mode_queries_props_only_for_loaded_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                { "id": "loaded-model", "status": { "value": "loaded" } },
                { "id": "idle-model", "status": { "value": "idle" } }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "loaded-model"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "n_ctx": 16384 })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(server.uri());
    let models = provider.list_models().await.expect("models parse");
    let by_id: std::collections::HashMap<_, _> = models
        .iter()
        .map(|model| (model.id.as_str(), model.input_token_limit))
        .collect();
    assert_eq!(by_id["loaded-model"], Some(16384));
    // Unloaded entries are never asked: asking could trigger an autoload.
    assert_eq!(by_id["idle-model"], None);
}

#[tokio::test]
async fn trailing_slashes_in_the_base_url_are_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{ "id": "m" }]
        })))
        .mount(&server)
        .await;

    let provider = LlamaCppProvider::new(format!("{}/", server.uri()));
    let models = provider.list_models().await.expect("models parse");
    assert_eq!(models, vec![card("m", None)]);
}
