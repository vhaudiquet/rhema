//! The [`LlamaCppProvider`]: HTTP and SSE transport for llama.cpp's
//! `llama-server` OpenAI-compatible API.
//!
//! One `POST {base}/v1/chat/completions` with `stream: true` per exchange;
//! the optional API key rides in the `Authorization: Bearer` header — never
//! a query parameter, so it never lands in URLs or access logs. Transport
//! failures map onto [`ProviderError`]: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Other`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`], unparseable
//! stream chunks and mid-stream error chunks to [`ProviderError::Api`] too.

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;

use crate::stream::SseDecoder;
use crate::wire::{ChatCompletionChunk, ChunkAssembler, WireErrorBody, WireModelsPage};

/// The `llama-server` default listen address.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// llama.cpp `llama-server` backend, speaking its OpenAI-compatible
/// chat-completions API.
pub struct LlamaCppProvider {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl LlamaCppProvider {
    /// A provider against `base_url` (trailing slashes trimmed) with no
    /// authentication — the default `llama-server` setup.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: None,
            http: reqwest::Client::new(),
        }
    }

    /// Authenticates with an `Authorization: Bearer` header, for servers
    /// started with `--api-key`. Header only: the key never goes in a URL.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// The models the server can serve (`GET /v1/models` → `data[].id`;
    /// llama.cpp lists the one loaded model).
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let response = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .authorized(&self.api_key)
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
        Ok(page.data.into_iter().map(|model| model.id).collect())
    }
}

/// Attaches the bearer header when a key is configured.
trait Authorized {
    fn authorized(self, api_key: &Option<String>) -> reqwest::RequestBuilder;
}

impl Authorized for reqwest::RequestBuilder {
    fn authorized(self, api_key: &Option<String>) -> reqwest::RequestBuilder {
        match api_key {
            Some(key) => self.bearer_auth(key),
            None => self,
        }
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it.
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api(format!(
        "llama.cpp api error (HTTP {}): {}",
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
impl ModelProvider for LlamaCppProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = crate::wire::request_body(&req);
        let url = format!("{}/v1/chat/completions", self.base_url);

        let response = self
            .http
            .post(&url)
            .authorized(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }

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
