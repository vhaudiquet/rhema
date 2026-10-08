//! The [`AnthropicProvider`]: HTTP and SSE transport for the Anthropic
//! Messages API.
//!
//! Auth is by API key in the `x-api-key` header — never a query parameter,
//! so the key never lands in URLs or access logs — with the pinned
//! `anthropic-version` header ([`ANTHROPIC_VERSION`]). Only the streaming
//! dialect is spoken; every call streams and assembles. Transport failures
//! map onto [`ProviderError`]: timeouts to [`ProviderError::Timeout`],
//! other network failures to [`ProviderError::Other`], non-2xx responses
//! (with the body's `error.message` when present) to [`ProviderError::Api`],
//! and unparseable stream chunks and mid-stream error events to
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
use rhema_provider_google::GeminiModel;

use crate::stream::SseDecoder;
use crate::wire::{ChunkAssembler, StreamEvent, WireErrorBody, WireModelsPage};

/// The production Anthropic API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The API version pin every request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The largest page size the models endpoint honors.
const MODELS_PAGE_LIMIT: u32 = 1000;

/// Anthropic backend, speaking the Messages API.
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    /// A provider authenticating with `api_key` against
    /// [`DEFAULT_BASE_URL`].
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
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for tests against
    /// a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Every model the key can see, following `has_more`/`after_id`
    /// pagination until the list is exhausted.
    pub async fn list_models(&self) -> Result<Vec<GeminiModel>, ProviderError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;
        loop {
            let mut request = self
                .authenticated(self.http.get(format!("{}/v1/models", self.base_url)))
                .query(&[("limit", MODELS_PAGE_LIMIT.to_string())])
                // The one overall timeout: a bounded catalog call, not a
                // stream of unbounded length.
                .timeout(Duration::from_secs(20));
            if let Some(after) = &after_id {
                request = request.query(&[("after_id", after)]);
            }
            let response = request.send().await.map_err(network_error)?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(api_error(status, &text));
            }
            let page: WireModelsPage = response
                .json()
                .await
                .map_err(|err| ProviderError::Api(format!("malformed models page: {err}")))?;
            let fetched = page.data.len();
            models.extend(page.data.into_iter().map(GeminiModel::from));
            // A missing cursor or a zero-row page keeps a misbehaving mirror
            // from spinning this loop forever.
            match page.has_more.then_some(page.last_id).flatten() {
                Some(cursor) if fetched > 0 => after_id = Some(cursor),
                _ => return Ok(models),
            }
        }
    }

    /// Common headers of every request: the key in `x-api-key` and the
    /// pinned API version.
    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the Anthropic
/// error shape's `error.message` when the body has it.
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api(format!(
        "anthropic api error (HTTP {}): {}",
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

/// Parses one SSE payload as a stream event and folds it into the
/// assembler. An empty payload (keep-alive) is skipped.
fn feed_chunk(
    payload: &str,
    assembler: &mut ChunkAssembler,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderError> {
    if payload.trim().is_empty() {
        return Ok(());
    }
    let event: StreamEvent = serde_json::from_str(payload)
        .map_err(|err| ProviderError::Api(format!("malformed SSE chunk: {err}")))?;
    assembler.push(event, sink)
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = crate::wire::request_body(&req)?;
        let url = format!("{}/v1/messages", self.base_url);

        let response = self
            .authenticated(self.http.post(&url))
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
                feed_chunk(&payload, &mut assembler, sink)?;
            }
        }
        for payload in decoder.finish() {
            feed_chunk(&payload, &mut assembler, sink)?;
        }
        Ok(assembler.finish())
    }
}
