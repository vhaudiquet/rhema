//! The live models.dev fetch. Offline unless run with `--ignored`.
#![cfg(feature = "live")]

use std::sync::Arc;

use async_trait::async_trait;
use rhema_model_catalog::{CatalogCache, Fetched, ModelsDev, Source};

#[tokio::test]
#[ignore = "hits the live models.dev API; run with `cargo test -p rhema-model-catalog -- --ignored`"]
async fn the_live_catalog_prices_a_known_model() {
    let cache = CatalogCache::new(Arc::new(ModelsDev::default()), None);
    let catalog = cache.get().await.expect("models.dev answers");
    let sonnet = catalog
        .facts("anthropic", "claude-sonnet-4-6")
        .expect("the row is there");
    assert!(sonnet.pricing.is_some());
    assert!(sonnet.input_token_limit.is_some());
}

/// A source that is always down, for the reload half.
struct Down;

#[async_trait]
impl Source for Down {
    async fn fetch(&self, _etag: Option<&str>) -> Result<Fetched, String> {
        Err("offline".to_owned())
    }
}

#[tokio::test]
#[ignore = "hits the live models.dev API; run with `cargo test -p rhema-model-catalog -- --ignored`"]
async fn the_live_catalog_round_trips_through_disk() {
    let path = std::env::temp_dir().join(format!("rhema-live-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cache = CatalogCache::new(Arc::new(ModelsDev::default()), Some(path.clone()));
    assert!(cache.get().await.is_some());

    let size = std::fs::metadata(&path)
        .expect("the cache file exists")
        .len();
    assert!(
        size > 100_000,
        "the projection keeps the rows: {size} bytes"
    );

    // A new process with no network still reads the facts.
    let reopened = CatalogCache::new(Arc::new(Down), Some(path.clone()));
    let catalog = reopened.get().await.expect("the disk copy loads");
    assert!(
        catalog
            .facts("anthropic", "claude-sonnet-4-6")
            .and_then(|facts| facts.pricing)
            .is_some()
    );
    let _ = std::fs::remove_file(path);
}
