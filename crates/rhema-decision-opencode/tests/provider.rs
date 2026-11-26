//! Offline integration tests: the provider against an in-process
//! wiremock server. Zero network.

use rhema_decision::{DecisionProvider, DecisionRequest, Question};
use rhema_decision_opencode::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, GO_BASE_URL, OpenCodeProvider, SYSTEMONE_PATH,
};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn the_default_routes_are_the_documented_endpoints() {
    assert_eq!(
        format!("{DEFAULT_BASE_URL}{SYSTEMONE_PATH}"),
        "https://opencode.ai/inference/systemone/v1/systemone"
    );
    assert_eq!(
        format!("{GO_BASE_URL}{SYSTEMONE_PATH}"),
        "https://opencode.ai/inference/go/systemone/v1/systemone"
    );
    assert_eq!(DEFAULT_MODEL, "jev-1.14");
}

#[tokio::test]
async fn the_call_rides_the_systemone_route_with_the_key_and_the_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(SYSTEMONE_PATH))
        .and(header("authorization", "Bearer oc_sk_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.14",
            "answers": {"q": {"type": "noul", "noul": 0.9}},
            "usage": {"input_tokens": 12, "output_tokens": 0}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("oc_sk_test")
        .with_base_url(server.uri())
        .with_model("gemini-decision-1");
    let request = DecisionRequest::new("charged twice").with_question("q", Question::noul("x?"));
    let response = provider.decide(&request).await.unwrap();
    assert_eq!(response.model, "jev-1.14");
    let received = server.received_requests().await.unwrap().remove(0);
    let body: serde_json::Value = serde_json::from_slice(&received.body).unwrap();
    assert_eq!(body["model"], "gemini-decision-1");
    assert_eq!(body["questions"]["q"]["type"], "noul");
}

#[tokio::test]
async fn a_go_key_against_the_go_route_is_a_subscription_route() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(SYSTEMONE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.14",
            "answers": {"q": {"type": "noul", "noul": 0.2}},
            "usage": {"input_tokens": 1, "output_tokens": 0}
        })))
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::go("oc_sk_test").with_base_url(server.uri());
    let request = DecisionRequest::new("s").with_question("q", Question::noul("x?"));
    assert!(provider.decide(&request).await.is_ok());
    assert_eq!(provider.kind(), "opencode");
}

#[tokio::test]
async fn the_gateways_error_envelope_is_invalid_with_its_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(SYSTEMONE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"type": "invalid_request_error", "message": "unknown model"}
        })))
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("k").with_base_url(server.uri());
    let request = DecisionRequest::new("s").with_question("q", Question::noul("x?"));
    let err = provider.decide(&request).await.unwrap_err();
    assert_eq!(
        err,
        rhema_decision::DecisionError::Invalid("unknown model".into())
    );
}
