//! The listing wrapper the provider registry installs.

use std::sync::Arc;

#[cfg(feature = "live")]
use async_trait::async_trait;
#[cfg(feature = "live")]
use rhema_core::ProviderError;
#[cfg(feature = "live")]
use rhema_provider::ModelCard;
use rhema_provider::ModelListing;

/// Wraps a provider's live listing so its rows gain the catalog's
/// facts after every fetch. A provider the catalog does not cover, or
/// a build without the `live` feature, passes through untouched.
pub fn decorate_listing(provider: &str, listing: Arc<dyn ModelListing>) -> Arc<dyn ModelListing> {
    #[cfg(feature = "live")]
    {
        if crate::provider_key(provider).is_none() {
            return listing;
        }
        Arc::new(LiveListing {
            inner: listing,
            provider: provider.to_owned(),
            cache: Arc::clone(crate::live::shared()),
        })
    }
    #[cfg(not(feature = "live"))]
    {
        let _ = provider;
        listing
    }
}

/// The one wrapper: fetch through, fill the blanks, never fail because
/// the catalog did.
#[cfg(feature = "live")]
struct LiveListing {
    /// The adapter's own listing.
    inner: Arc<dyn ModelListing>,
    /// The config spelling the catalog maps to a key.
    provider: String,
    /// The process-wide fetch and its cache.
    cache: Arc<crate::live::CatalogCache>,
}

#[cfg(feature = "live")]
#[async_trait]
impl ModelListing for LiveListing {
    /// The adapter's rows, with the catalog's facts in their blanks.
    /// A failed fetch leaves the rows exactly as the adapter returned
    /// them.
    ///
    /// # Errors
    ///
    /// The adapter's own errors pass through unchanged; the catalog
    /// adds no failure of its own.
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        // The provider and the catalog answer independently: run them
        // together so the catalog's fetch never adds to a slow
        // provider's listing.
        let (models, catalog) = tokio::join!(self.inner.list_models(), self.cache.get());
        let mut models = models?;
        if let Some(catalog) = catalog {
            catalog.decorate(&self.provider, &mut models);
        }
        Ok(models)
    }
}

#[cfg(all(test, feature = "live"))]
mod tests {
    use super::*;
    use crate::live::{CatalogCache, Fetched, Source};
    use rhema_provider::Pricing;

    const BODY: &str = r#"{
      "anthropic": { "models": {
        "claude-sonnet-4-6": {
          "name": "Claude Sonnet 4.6",
          "cost": { "input": 3, "output": 15, "cache_read": 0.3 },
          "limit": { "context": 1000000, "output": 128000 },
          "modalities": { "input": ["text", "image"] }
        }
      }}
    }"#;

    /// A source with one fixed body.
    struct Fixed;

    #[async_trait]
    impl Source for Fixed {
        async fn fetch(&self, _etag: Option<&str>) -> Result<Fetched, String> {
            Ok(Fetched::Fresh {
                body: BODY.to_owned(),
                etag: None,
            })
        }
    }

    /// A source that is always down.
    struct Down;

    #[async_trait]
    impl Source for Down {
        async fn fetch(&self, _etag: Option<&str>) -> Result<Fetched, String> {
            Err("offline".to_owned())
        }
    }

    fn cache(source: Arc<dyn Source>) -> Arc<CatalogCache> {
        Arc::new(CatalogCache::new(source, None))
    }

    /// A listing with one card, its identity the only thing known.
    struct Bare(&'static str);

    #[async_trait]
    impl ModelListing for Bare {
        async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
            Ok(vec![ModelCard {
                id: self.0.to_owned(),
                display_name: self.0.to_owned(),
                description: String::new(),
                input_token_limit: None,
                output_token_limit: None,
                supports_vision: None,
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
            }])
        }
    }

    /// A listing that fails: the wrapper must not swallow it.
    struct Broken;

    #[async_trait]
    impl ModelListing for Broken {
        async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
            Err(ProviderError::Api {
                message: "listing down".to_owned(),
                status: None,
            })
        }
    }

    fn listing(
        provider: &str,
        inner: Arc<dyn ModelListing>,
        cache: Arc<CatalogCache>,
    ) -> LiveListing {
        LiveListing {
            inner,
            provider: provider.to_owned(),
            cache,
        }
    }

    #[tokio::test]
    async fn the_wrapper_fills_the_live_rows() {
        let live = listing(
            "anthropic",
            Arc::new(Bare("claude-sonnet-4-6")),
            cache(Arc::new(Fixed)),
        );
        let models = live.list_models().await.expect("the bare listing answers");
        let card = &models[0];
        assert_eq!(card.display_name, "Claude Sonnet 4.6");
        assert_eq!(
            card.pricing,
            Some(Pricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            })
        );
        assert_eq!(card.input_token_limit, Some(1_000_000));
        assert_eq!(card.supports_vision, Some(true));
    }

    #[tokio::test]
    async fn a_failed_catalog_fetch_leaves_the_listing_as_it_came() {
        let live = listing(
            "anthropic",
            Arc::new(Bare("claude-sonnet-4-6")),
            cache(Arc::new(Down)),
        );
        let models = live.list_models().await.expect("the listing still answers");
        assert_eq!(models[0].display_name, "claude-sonnet-4-6");
        assert!(models[0].pricing.is_none());
        assert!(models[0].input_token_limit.is_none());
    }

    #[tokio::test]
    async fn a_failed_listing_stays_failed() {
        let live = listing("anthropic", Arc::new(Broken), cache(Arc::new(Fixed)));
        assert!(live.list_models().await.is_err());
    }

    #[tokio::test]
    async fn an_unwired_provider_passes_through() {
        let listing = decorate_listing("llama-cpp", Arc::new(Bare("local")));
        let models = listing
            .list_models()
            .await
            .expect("the bare listing answers");
        assert_eq!(models[0].display_name, "local");
        assert!(models[0].pricing.is_none());
    }
}
