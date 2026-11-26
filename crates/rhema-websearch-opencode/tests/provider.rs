//! Offline integration tests: the provider against an in-process
//! wiremock server. Zero network.

use rhema_websearch::{SearchError, SearchQuery, WebSearchProvider};
use rhema_websearch_opencode::{DEFAULT_BASE_URL, OpenCodeProvider, PROVIDER_ID, WEBSEARCH_PATH};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn the_default_route_is_the_documented_endpoint() {
    assert_eq!(
        format!("{DEFAULT_BASE_URL}{WEBSEARCH_PATH}"),
        "https://opencode.ai/console/api/websearch"
    );
    assert_eq!(PROVIDER_ID, "opencode");
}

#[tokio::test]
async fn the_call_carries_the_key_and_the_provider_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WEBSEARCH_PATH))
        .and(header("authorization", "Bearer oc_sk_test"))
        .and(body_json(
            json!({"query": "rust async", "providerID": "opencode"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "providerID": "opencode",
            "results": [
                {"url": "https://a", "title": "Async Book", "content": "Getting started"},
                {"url": "https://b", "title": "Tokio", "content": "A runtime"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("oc_sk_test").with_base_url(server.uri());
    let results = provider
        .search(&SearchQuery::new("rust async"))
        .await
        .expect("the call succeeds");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Async Book");
    assert_eq!(results[0].url, "https://a");
    assert_eq!(results[0].snippet, "Getting started");
}

#[tokio::test]
async fn the_caller_count_trims_the_gateways_eight() {
    let server = MockServer::start().await;
    let results = (0..8)
        .map(|i| json!({"url": format!("https://{i}"), "title": format!("hit {i}")}))
        .collect::<Vec<_>>();
    Mock::given(method("POST"))
        .and(path(WEBSEARCH_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"providerID": "opencode", "results": results})),
        )
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("k").with_base_url(server.uri());
    let results = provider
        .search(&SearchQuery::new("q").with_max_results(3))
        .await
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].url, "https://0");
}

#[tokio::test]
async fn another_providers_answer_is_a_parse_error_with_its_names() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WEBSEARCH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "providerID": "tavily",
            "results": [{"url": "https://a", "title": "x"}]
        })))
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("k").with_base_url(server.uri());
    let err = provider.search(&SearchQuery::new("q")).await.unwrap_err();
    assert!(matches!(err, SearchError::Parse(_)), "{err:?}");
}

#[tokio::test]
async fn a_workspace_without_hosted_search_is_an_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WEBSEARCH_PATH))
        .respond_with(ResponseTemplate::new(402).set_body_json(json!({
            "error": {"message": "hosted web search is not enabled"}
        })))
        .mount(&server)
        .await;

    let provider = OpenCodeProvider::new("k").with_base_url(server.uri());
    let err = provider.search(&SearchQuery::new("q")).await.unwrap_err();
    let SearchError::Http { status, .. } = err else {
        panic!("expected Http, got {err:?}");
    };
    assert_eq!(status, 402);
}
