//! Offline integration tests: the provider against an in-process
//! wiremock server. Zero network.

use rhema_core::{
    ChatParams, Content, Event, EventSink, Message, ModelProvider, ModelRequest, Role, StopReason,
};
use rhema_provider::ModelListing;
use rhema_provider_opencode::{OpenCodeProvider, SESSION_HEADER, USER_AGENT};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Collector(Vec<String>);

impl EventSink for Collector {
    fn emit(&mut self, event: Event<'_>) {
        self.0.push(format!("{event:?}"));
    }
}

fn provider(server: &MockServer) -> OpenCodeProvider {
    OpenCodeProvider::new("oc_sk_test")
        .with_session_id("ses_test")
        .with_base_url(server.uri())
}

fn request() -> ModelRequest {
    ModelRequest {
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        params: ChatParams::new("deepseek-v4.1-flash"),
    }
}

#[tokio::test]
async fn the_catalog_reads_console_facts_and_drops_free_rows() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "object": "list",
        "data": [
            {
                "id": "deepseek-v4.1-flash",
                "name": "DeepSeek V4.1 Flash",
                "description": "Reasoning and agentic coding",
                "type": "chat",
                "modalities": { "input": ["text", "image/png"] },
                "limit": { "context": 1_000_000, "output": 384_000 },
                "cost": { "text": {
                    "input": { "price": 0.3, "cache": { "read": 0.006 } },
                    "output": { "price": 1.2 }
                }}
            },
            {
                "id": "free-model",
                "name": "Free",
                "type": "chat",
                "cost": { "text": {
                    "input": { "price": 0.0 },
                    "output": { "price": 0.0 }
                }}
            },
            { "id": "embed", "name": "Embedding", "type": "embedding" }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header(SESSION_HEADER, "ses_test"))
        .and(header("authorization", "Bearer oc_sk_test"))
        .and(header("user-agent", USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let models = provider(&server)
        .list_models()
        .await
        .expect("the catalog answers");

    assert_eq!(models.len(), 1, "free and non-chat rows are dropped");
    let card = &models[0];
    assert_eq!(card.id, "deepseek-v4.1-flash");
    assert_eq!(card.input_token_limit, Some(1_000_000));
    assert_eq!(card.output_token_limit, Some(384_000));
    assert_eq!(card.supports_vision, Some(true));
    let pricing = card.pricing.expect("priced");
    assert_eq!(pricing.input_per_mtok, 0.3);
    assert_eq!(pricing.output_per_mtok, 1.2);
    assert_eq!(pricing.cached_input_per_mtok, Some(0.006));
}

#[tokio::test]
async fn a_go_base_keeps_the_free_rows_go_includes() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "data": [{
            "id": "free-model",
            "name": "Free",
            "type": "chat",
            "cost": { "text": {
                "input": { "price": 0.0 },
                "output": { "price": 0.0 }
            }}
        }]
    });
    Mock::given(method("GET"))
        .and(path("/inference/go/openai/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let base = format!("{}/inference/go/openai/v1", server.uri());
    let models = OpenCodeProvider::new("k")
        .with_base_url(base)
        .list_models()
        .await
        .expect("the catalog answers");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "free-model");
}

#[tokio::test]
async fn completions_carry_the_session_header_and_use_the_shared_core() {
    let server = MockServer::start().await;
    let body = [
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\",\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"role\":\"assistant\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header(SESSION_HEADER, "ses_test"))
        .and(header("authorization", "Bearer oc_sk_test"))
        .and(header("user-agent", USER_AGENT))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let output = provider(&server)
        .complete(request(), &mut sink)
        .await
        .expect("the stream completes");

    assert_eq!(output.message.content, vec![Content::Text("ok".into())]);
    assert_eq!(output.stop_reason, StopReason::EndTurn);
    assert_eq!(output.usage.input_tokens, 3);
    assert_eq!(output.usage.output_tokens, 1);

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], "deepseek-v4.1-flash");
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn a_non_2xx_response_keeps_the_status_and_console_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": { "message": "free models require OpenCode" }
        })))
        .mount(&server)
        .await;

    let mut sink = Collector(vec![]);
    let error = provider(&server)
        .complete(request(), &mut sink)
        .await
        .expect_err("403 fails the call");
    let message = error.to_string();
    assert!(
        message.contains("free models require OpenCode"),
        "{message}"
    );
    match error {
        rhema_core::ProviderError::Api { status, .. } => assert_eq!(status, Some(403)),
        other => panic!("expected an api error, got {other:?}"),
    }
}
