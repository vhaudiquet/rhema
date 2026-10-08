//! The [`OpenAiCompatibleProvider`]: HTTP and SSE transport for any
//! OpenAI-compatible chat-completions API.
//!
//! The base URL is the full versioned base — e.g. `https://api.openai.com/v1`
//! or a local server's `/v1` — and is used verbatim: the endpoints are
//! `{base}/chat/completions` and `{base}/models`, with nothing appended.
//! Trailing slashes are trimmed, so either spelling of a base works.
//!
//! Auth is by API key in the `Authorization: Bearer` header — never a query
//! parameter, so the key never lands in URLs or access logs. An empty key
//! omits the header entirely, for local servers that need no auth.
//! Transport failures map onto [`ProviderError`]: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Other`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`], unparseable
//! stream chunks and mid-stream error chunks to [`ProviderError::Api`] too.
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
use crate::wire::{ChatCompletionChunk, ChunkAssembler, WireErrorBody};

/// The production OpenAI API base, version segment included.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// The HTTP client every instance shares: connect capped, stream reads
/// stall-guarded, never total-time capped.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // One connect attempt; a dead network fails fast instead of
        // hanging the agent step.
        .connect_timeout(Duration::from_secs(10))
        // Per-read stall guard for streams: bounds the silence between
        // chunks, never the total generation time.
        .read_timeout(Duration::from_secs(90))
        .build()
        .expect("a client builder with only timeouts cannot fail")
}

/// Any server speaking the OpenAI chat-completions dialect, addressed by
/// its full versioned base URL.
pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    /// A provider authenticating with `api_key` against [`DEFAULT_BASE_URL`].
    ///
    /// An empty key is accepted but useless against OpenAI: the header is
    /// omitted and the API answers 401. Local compatible servers commonly
    /// want exactly that.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: api_key.into(),
            http: http_client(),
        }
    }

    /// Targets `base_url` — the FULL versioned base (e.g.
    /// `https://api.openai.com/v1`, `http://localhost:8080/v1`), used
    /// verbatim with the endpoint paths appended; trailing slashes are
    /// trimmed. This is the constructor for every compatible server that is
    /// not OpenAI itself.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// The models the server can serve (`GET {base}/models` → `data[].id`,
    /// plus whatever metadata the entry carries). Sparse entries are the
    /// norm on generic servers; see [`crate::wire::normalize_models`].
    pub async fn list_models(&self) -> Result<Vec<GeminiModel>, ProviderError> {
        let response = self
            .http
            .get(format!("{}/models", self.base_url))
            .authorized(&self.api_key)
            // The one overall timeout: a bounded listing call, not a stream
            // of unbounded length.
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let page: serde_json::Value = response
            .json()
            .await
            .map_err(|err| ProviderError::Api(format!("malformed models page: {err}")))?;
        Ok(crate::wire::normalize_models(&page))
    }
}

/// Attaches the bearer header when a key is configured; an empty key means
/// an anonymous request (local servers).
trait Authorized {
    fn authorized(self, api_key: &str) -> reqwest::RequestBuilder;
}

impl Authorized for reqwest::RequestBuilder {
    fn authorized(self, api_key: &str) -> reqwest::RequestBuilder {
        if api_key.is_empty() {
            self
        } else {
            self.bearer_auth(api_key)
        }
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it.
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api(format!(
        "openai-compatible api error (HTTP {}): {}",
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
impl ModelProvider for OpenAiCompatibleProvider {
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
