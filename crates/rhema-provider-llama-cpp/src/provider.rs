//! The [`LlamaCppProvider`]: HTTP and SSE transport for llama.cpp's
//! `llama-server` OpenAI-compatible API.
//!
//! One `POST {base}/v1/chat/completions` with `stream: true` per exchange;
//! the optional API key rides in the `Authorization: Bearer` header — never
//! a query parameter, so it never lands in URLs or access logs. Transport
//! failures map onto [`ProviderError`]: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Network`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`] with the HTTP
//! status, and unparseable stream chunks and mid-stream error chunks to
//! [`ProviderError::Api`] without one.

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;

use rhema_provider::{ModelCard, SseDecoder};

use crate::wire::{ChatCompletionChunk, ChunkAssembler, WireErrorBody};

/// The `llama-server` default listen address.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// llama.cpp `llama-server` backend, speaking its OpenAI-compatible
/// chat-completions API.
#[derive(Debug)]
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
            // A dead server must fail fast: connects cap at 10s, and a
            // stream that stops producing bytes for 90s is dead too.
            // Streaming calls get no total timeout — generations can run
            // for minutes.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("a client builder with only timeouts cannot fail"),
        }
    }

    /// Authenticates with an `Authorization: Bearer` header, for servers
    /// started with `--api-key`. Header only: the key never goes in a URL.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// The models the server can serve, normalized into the shared
    /// [`ModelCard`] rows (`GET /v1/models`; llama.cpp lists the one
    /// loaded model). A single-model server also gets its context from
    /// `/props`; in router mode, models that are loaded and still silent
    /// about their context are asked per model (bounded — never for
    /// unloaded entries, which some servers would autoload just to
    /// answer).
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx listing, and [`ProviderError::Api`] without
    /// one on a malformed listing or props body.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let response = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .authorized(&self.api_key)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let page: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed models page: {err}"),
            status: None,
        })?;
        let mut models = crate::wire::normalize_models(&page);

        if models.len() == 1 && models[0].context_length.is_none() {
            if let Some(n_ctx) = self.props(None).await? {
                models[0].context_length = Some(n_ctx);
            }
        } else {
            let silent: Vec<String> = models
                .iter()
                .filter(|model| model.loaded == Some(true) && model.context_length.is_none())
                .take(8)
                .map(|model| model.id.clone())
                .collect();
            for id in silent {
                if let Some(n_ctx) = self.props(Some(&id)).await? {
                    if let Some(model) = models.iter_mut().find(|model| model.id == id) {
                        model.context_length = Some(n_ctx);
                    }
                }
            }
        }
        Ok(models.into_iter().map(ModelCard::from).collect())
    }

    /// The server's runtime properties, optionally for one model. `n_ctx`
    /// sits at the top level on single-model servers; the router proxies
    /// per-model queries to the instance.
    async fn props(&self, model: Option<&str>) -> Result<Option<u64>, ProviderError> {
        let mut url = format!("{}/props", self.base_url);
        if let Some(model) = model {
            url.push_str(&format!("?model={}", urlencode(model)));
        }
        let response = self
            .http
            .get(url)
            .authorized(&self.api_key)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            return Ok(None);
        }
        let props: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed props: {err}"),
            status: None,
        })?;
        Ok(crate::wire::context_from_props(&props))
    }
}

/// Percent-encodes a model id for a query string. Ids are names, but
/// routers have been seen serving ids with slashes and spaces.
fn urlencode(model: &str) -> String {
    let mut encoded = String::new();
    for byte in model.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
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
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        message: format!(
            "llama.cpp api error (HTTP {}): {}",
            status.as_u16(),
            parsed.message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot be
/// carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(err.to_string())
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
    let chunk: ChatCompletionChunk =
        serde_json::from_str(payload).map_err(|err| ProviderError::Api {
            message: format!("malformed SSE chunk: {err}"),
            status: None,
        })?;
    assembler.push(chunk, sink)
}

#[async_trait]
impl ModelProvider for LlamaCppProvider {
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
