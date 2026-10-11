//! The [`OpenRouterProvider`]: HTTP and SSE transport for OpenRouter's
//! OpenAI-compatible API.
//!
//! Auth is by API key in the `Authorization: Bearer` header — never a query
//! parameter, so the key never lands in URLs or access logs. Every request
//! also carries OpenRouter's recommended app-attribution headers
//! ([`APP_REFERER`], [`APP_TITLE`]). Transport failures map onto
//! [`ProviderError`]: timeouts to [`ProviderError::Timeout`], other network
//! failures to [`ProviderError::Other`], non-2xx responses (with the
//! body's `error.message` when present) to [`ProviderError::Api`],
//! unparseable stream chunks and mid-stream error chunks to
//! [`ProviderError::Api`] too.
//!
//! Timeouts are layered: the client bounds connection setup (10s) and each
//! stream read (90s — a stall guard, not a duration cap, because a long
//! generation is legitimate), and non-streaming calls (the models list)
//! additionally carry a 20s overall request timeout. Streaming calls get no
//! total timeout for the same reason.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{Allowance, ModelCard, SseDecoder};
use serde::de::DeserializeOwned;

use crate::wire::{
    ChatCompletionChunk, ChunkAssembler, WireCredits, WireErrorBody, WireKey, WireModelsPage,
    is_text,
};

/// The production OpenRouter API base.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The `HTTP-Referer` attribution header OpenRouter recommends for its
/// rankings on openrouter.ai.
pub const APP_REFERER: &str = "https://github.com/vhaudiquet/rhema";

/// The `X-Title` attribution header OpenRouter recommends for its rankings
/// on openrouter.ai.
pub const APP_TITLE: &str = "rhema";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// The largest page size OpenRouter's models endpoint honors.
const MODELS_PAGE_LIMIT: u32 = 1000;

/// OpenRouter backend, speaking its OpenAI-compatible chat-completions API.
pub struct OpenRouterProvider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
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
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: api_key.into(),
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for tests against
    /// a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Every text chat model OpenRouter offers, following `limit`/`offset`
    /// pagination until `links.next` is exhausted. The catalog is public:
    /// the call works without an API key.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
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
            let page: WireModelsPage = response
                .json()
                .await
                .map_err(|err| ProviderError::Api(format!("malformed models page: {err}")))?;
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

    /// The account's prepaid credit: `GET /credits` for the account's
    /// total and spend, `GET /auth/key` for the calling key's own cap —
    /// the [`Allowance`] is the smaller headroom of the two. Both
    /// endpoints authenticate with the API key; an empty key answers
    /// 401 and the call fails.
    pub async fn allowance(&self) -> Result<Allowance, ProviderError> {
        let credits: WireCredits = self.get_json("/credits").await?;
        let key: WireKey = self.get_json("/auth/key").await?;
        crate::wire::allowance(&credits, &key)
            .ok_or_else(|| ProviderError::Api("credit response carries no total".to_owned()))
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
        response
            .json()
            .await
            .map_err(|err| ProviderError::Api(format!("malformed {path} response: {err}")))
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
/// error shape's `error.message` when the body has it.
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api(format!(
        "openrouter api error (HTTP {}): {}",
        status.as_u16(),
        parsed.message(raw)
    ))
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot be
/// carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Other(format!("network error: {err}"))
    }
}

/// Parses one SSE payload as a chunk and folds it into the assembler. An
/// empty payload (keep-alive) is skipped.
fn feed_chunk(
    payload: &str,
    assembler: &mut ChunkAssembler,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderError> {
    if payload.trim().is_empty() {
        return Ok(());
    }
    let chunk: ChatCompletionChunk = serde_json::from_str(payload)
        .map_err(|err| ProviderError::Api(format!("malformed SSE chunk: {err}")))?;
    assembler.push(chunk, sink)
}

#[async_trait]
impl ModelProvider for OpenRouterProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = crate::wire::request_body(&req);
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .http
            .post(&url)
            .attributed(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }

        // No overall timeout: a long generation is legitimate, and the
        // client's read timeout already bounds stalls between chunks.
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut assembler = ChunkAssembler::new();
        while let Some(item) = stream.next().await {
            let bytes = item.map_err(network_error)?;
            for payload in decoder.push(&bytes) {
                if payload.trim() == DONE_SENTINEL {
                    return Ok(assembler.finish());
                }
                feed_chunk(&payload, &mut assembler, sink)?;
            }
        }
        for payload in decoder.finish() {
            if payload.trim() == DONE_SENTINEL {
                break;
            }
            feed_chunk(&payload, &mut assembler, sink)?;
        }
        Ok(assembler.finish())
    }
}
