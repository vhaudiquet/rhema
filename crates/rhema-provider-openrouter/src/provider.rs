//! The [`OpenRouterProvider`]: HTTP transport for OpenRouter's catalog
//! and account endpoints, with completions driven through the shared
//! dialect core
//! ([`rhema_provider_openai_compatible`]) speaking OpenRouter's
//! [`crate::wire::OPENROUTER`] dialect.
//!
//! Auth is by API key in the `Authorization: Bearer` header — never a query
//! parameter, so the key never lands in URLs or access logs. Every request
//! also carries OpenRouter's recommended app-attribution headers
//! ([`APP_REFERER`], [`APP_TITLE`]). Transport failures map onto
//! [`ProviderError`] exactly as the shared core completes: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Network`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`], unparseable
//! stream chunks and mid-stream error chunks to [`ProviderError::Api`]
//! too.
//!
//! Timeouts are layered: the client bounds connection setup (10s) and each
//! stream read (90s — a stall guard, not a duration cap, because a long
//! generation is legitimate), and non-streaming calls (the models list)
//! additionally carry a 20s overall request timeout. Streaming calls get no
//! total timeout for the same reason.

use std::time::Duration;

use async_trait::async_trait;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{Allowance, ModelCard, ModelListing};
use rhema_provider_openai_compatible::provider::{
    OpenAiCompatibleProvider, network_error as dialect_network_error,
};
use rhema_provider_openai_compatible::wire::WireErrorBody;
use serde::de::DeserializeOwned;

use crate::wire::{OPENROUTER, WireCredits, WireKey, WireModelsPage, is_text};

/// The production OpenRouter API base.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The `HTTP-Referer` attribution header OpenRouter recommends for its
/// rankings on openrouter.ai.
pub const APP_REFERER: &str = "https://rhema.rs";

/// The `X-Title` attribution header OpenRouter recommends for its rankings
/// on openrouter.ai.
pub const APP_TITLE: &str = "rhema";

/// The largest page size OpenRouter's models endpoint honors.
const MODELS_PAGE_LIMIT: u32 = 1000;

/// OpenRouter backend, speaking its OpenAI-compatible chat-completions API.
pub struct OpenRouterProvider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// The dialect-core transport carrying OpenRouter's dialect and
    /// attribution headers; every completion goes through it.
    transport: OpenAiCompatibleProvider,
}

impl std::fmt::Debug for OpenRouterProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never prints; the plain data does.
        f.debug_struct("OpenRouterProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("http", &self.http)
            .finish()
    }
}

impl OpenRouterProvider {
    /// A provider authenticating with `api_key` against [`DEFAULT_BASE_URL`].
    ///
    /// An empty key is accepted but useless for completions: the header is
    /// omitted and the API answers 401. Listing models works without a key.
    pub fn new(api_key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt; a dead network fails fast instead of
            // hanging the agent step.
            .connect_timeout(Duration::from_secs(10))
            // Per-read stall guard for streams: bounds the silence between
            // chunks, never the total generation time.
            .read_timeout(Duration::from_secs(90))
            .build()
            .expect("a client builder with only timeouts cannot fail");
        let api_key = api_key.into();
        Self {
            transport: transport_for(DEFAULT_BASE_URL, &api_key),
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key,
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for tests against
    /// a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self.transport = transport_for(&self.base_url, &self.api_key);
        self
    }

    /// The account's prepaid credit: `GET /credits` for the account's
    /// total and spend, `GET /auth/key` for the calling key's own cap —
    /// the [`Allowance`] is the smaller headroom of the two. Both
    /// endpoints authenticate with the API key; an empty key answers
    /// 401 and the call fails.
    pub async fn allowance(&self) -> Result<Allowance, ProviderError> {
        let credits: WireCredits = self.get_json("/credits").await?;
        let key: WireKey = self.get_json("/auth/key").await?;
        crate::wire::allowance(&credits, &key).ok_or_else(|| ProviderError::Api {
            message: "credit response carries no total".to_owned(),
            status: None,
        })
    }

    /// GETs one authenticated JSON body off the base URL, under the
    /// listing-style 20s overall timeout.
    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ProviderError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .attributed(&self.api_key)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed {path} response: {err}"),
            status: None,
        })
    }
}

/// Common headers of every request: the bearer header when a key is
/// configured, plus the app-attribution pair OpenRouter's docs recommend.
trait Attributed {
    fn attributed(self, api_key: &str) -> reqwest::RequestBuilder;
}

impl Attributed for reqwest::RequestBuilder {
    fn attributed(self, api_key: &str) -> reqwest::RequestBuilder {
        let request = self
            .header("HTTP-Referer", APP_REFERER)
            .header("X-Title", APP_TITLE);
        if api_key.is_empty() {
            request
        } else {
            request.bearer_auth(api_key)
        }
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenRouter
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        message: format!(
            "openrouter api error (HTTP {}): {}",
            status.as_u16(),
            parsed.message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot be
/// carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    dialect_network_error(err)
}

/// The dialect-core transport for one OpenRouter base: its dialect,
/// its attribution headers, the bearer key.
fn transport_for(base_url: &str, api_key: &str) -> OpenAiCompatibleProvider {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("HTTP-Referer", APP_REFERER.parse().expect("a header value"));
    headers.insert("X-Title", APP_TITLE.parse().expect("a header value"));
    OpenAiCompatibleProvider::new(api_key)
        .with_base_url(base_url)
        .with_dialect(OPENROUTER)
        .with_extra_headers(headers)
}

#[async_trait]
impl ModelListing for OpenRouterProvider {
    /// Every text chat model OpenRouter offers, following `limit`/`offset`
    /// pagination until `links.next` is exhausted. The catalog is public:
    /// the call works without an API key.
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let mut models = Vec::new();
        let mut offset: u32 = 0;
        loop {
            let response = self
                .http
                .get(format!("{}/models", self.base_url))
                .query(&[
                    ("limit", MODELS_PAGE_LIMIT.to_string()),
                    ("offset", offset.to_string()),
                ])
                .attributed(&self.api_key)
                // The one overall timeout: a bounded catalog call, not a
                // stream of unbounded length.
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .map_err(network_error)?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(api_error(status, &text));
            }
            let page: WireModelsPage = response.json().await.map_err(|err| ProviderError::Api {
                message: format!("malformed models page: {err}"),
                status: None,
            })?;
            // Linked before the models move out of the page.
            let more = page.next_page();
            let fetched = page.data.len();
            models.extend(page.data.into_iter().map(ModelCard::from).filter(is_text));
            // The zero-row guard keeps a misbehaving mirror from spinning
            // this loop forever.
            if fetched == 0 || !more {
                return Ok(models);
            }
            offset += fetched as u32;
        }
    }
}

#[async_trait]
impl ModelProvider for OpenRouterProvider {
    /// # Errors
    ///
    /// Fails with [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx response, and [`ProviderError::Api`] without
    /// one on a malformed stream chunk or a mid-stream error chunk.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.transport.complete(req, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_prints_presence_never_the_key() {
        let provider = OpenRouterProvider::new("sk-or-secret");
        let printed = format!("{provider:?}");
        assert!(!printed.contains("sk-or-secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // The plain data stays visible.
        assert!(printed.contains("base_url"), "{printed}");
    }
}
