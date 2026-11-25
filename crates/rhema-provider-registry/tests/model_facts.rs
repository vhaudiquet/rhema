//! Offline integration test: the registry's listing carries the
//! models.dev facts, and a fact an adapter measured itself wins.

use std::sync::Arc;

use rhema_model_catalog::{CatalogCache, ModelsDev, install_for_tests};
use rhema_provider_registry::build;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Z.ai's `/v1/models` page: ids and display names, no facts.
fn models_page() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": [
            { "type": "model", "id": "glm-4.5", "display_name": "GLM-4.5" },
            { "type": "model", "id": "glm-5.3", "display_name": "GLM-5.3" }
        ],
        "has_more": false
    }))
}

/// The models.dev rows the decorator should find.
fn catalog_page() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "zai": { "models": {
            "glm-4.5": {
                "name": "GLM-4.5",
                "cost": { "input": 0.6, "output": 2.2, "cache_read": 0.11 },
                "limit": { "context": 131072, "output": 98304 },
                "modalities": { "input": ["text"] }
            },
            "glm-5.3": {
                "name": "GLM-5.3",
                "cost": { "input": 1.4, "output": 4.4 },
                "limit": { "context": 1000000, "output": 131072 },
                "modalities": { "input": ["text"] }
            }
        }},
        "opencode": { "models": {} }
    }))
}

#[tokio::test]
async fn the_listing_gains_the_catalog_facts_and_keeps_its_own() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(catalog_page())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer key"))
        .respond_with(models_page())
        .mount(&server)
        .await;

    // The live cache points at the mock, not models.dev.
    let cache = CatalogCache::new(
        Arc::new(ModelsDev::new(format!("{}/api.json", server.uri()))),
        None,
    );
    assert!(install_for_tests(Arc::new(cache)), "first install");

    let built = build("zai", Some("key"), None, Some(&server.uri()), None).expect("zai builds");

    let models = built
        .listing
        .list_models()
        .await
        .expect("the listing answers");
    let glm_45 = models
        .iter()
        .find(|model| model.id == "glm-4.5")
        .expect("the row is listed");
    // The adapter knows nothing about GLM-4.5: the catalog prices it,
    // sizes it, and reads its text-only modality.
    assert_eq!(glm_45.display_name, "GLM-4.5");
    assert_eq!(glm_45.input_token_limit, Some(131_072));
    assert_eq!(glm_45.output_token_limit, Some(98_304));
    assert_eq!(glm_45.supports_vision, Some(false));
    let pricing = glm_45.pricing.expect("the catalog prices it");
    assert_eq!(pricing.input_per_mtok, 0.6);
    assert_eq!(pricing.output_per_mtok, 2.2);

    // Every row that has a catalog entry gains one, not just the first:
    // the decorator walks the whole page. GLM-5.3 is text-only in
    // models.dev; the adapter must not have asserted a vision verdict
    // that this could not correct.
    let glm_53 = models
        .iter()
        .find(|model| model.id == "glm-5.3")
        .expect("the row is listed");
    assert_eq!(glm_53.output_token_limit, Some(131_072));
    assert_eq!(glm_53.supports_vision, Some(false));
    assert_eq!(glm_53.pricing.expect("priced").input_per_mtok, 1.4);
}
