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
use rhema_provider::{Allowance, ModelCard, ModelListing, Pricing, SseDecoder};

use crate::wire::{ChatCompletionChunk, ChunkAssembler, WireBalance, WireErrorBody, is_text};

/// The production OpenAI API base, version segment included.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The SSE payload that ends every OpenAI-compatible chat stream.
pub const DONE_SENTINEL: &str = "[DONE]";

/// The HTTP client every instance shares: connect capped, stream reads
/// stall-guarded, never total-time capped.
pub fn http_client() -> reqwest::Client {
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

/// DeepSeek's OpenAI-compatible endpoint — the base the `deepseek`
/// kind speaks by default, over the generic adapter.
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// Any server speaking the OpenAI chat-completions dialect, addressed by
/// its full versioned base URL.
pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// The wire knobs: the shared vocabulary is this crate's; the
    /// dialect names the keys where the addressed server family spells
    /// its deviations.
    dialect: crate::wire::Dialect,
    /// Extra headers every request carries — routing metadata,
    /// editor identification, whatever the service demands.
    extra_headers: reqwest::header::HeaderMap,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never prints; the plain data does.
        f.debug_struct("OpenAiCompatibleProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("http", &self.http)
            .finish()
    }
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
            dialect: crate::wire::OPENAI_COMPATIBLE,
            extra_headers: reqwest::header::HeaderMap::new(),
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

    /// Speaks another [`Dialect`] — another server family's spelling of
    /// the same chat-completions vocabulary. This is how
    /// `rhema-provider-openrouter` and `…-llama-cpp` reuse this
    /// transport: their providers hold one of these, dialed to their
    /// dialect.
    pub fn with_dialect(mut self, dialect: crate::wire::Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Attaches headers every request carries. Called repeatedly, the
    /// maps merge; later values win per header name.
    pub fn with_extra_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.extra_headers.extend(headers);
        self
    }

    /// The DeepSeek account's prepaid balance (`GET /user/balance`).
    /// The endpoint is served beside the versioned base, so the `/v1`
    /// segment comes off first; every other compatible server has no
    /// such endpoint and callers only ask it of DeepSeek backends.
    pub async fn balance(&self) -> Result<Allowance, ProviderError> {
        let base = self.base_url.strip_suffix("/v1").unwrap_or(&self.base_url);
        let response = self
            .http
            .get(format!("{base}/user/balance"))
            .authorized(&self.api_key, &self.extra_headers)
            // A bounded listing-style call, not a stream.
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let balance: WireBalance = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed balance response: {err}"),
            status: None,
        })?;
        crate::wire::allowance(&balance).ok_or_else(|| ProviderError::Api {
            message: "balance response carries no balance".into(),
            status: None,
        })
    }
}

/// The api.deepseek.com host — the one endpoint this adapter decorates
/// with a published catalog, because their `/models` serves ids alone.
const DEEPSEEK_HOST: &str = "api.deepseek.com";

/// One row of DeepSeek's published catalog: context window, output
/// ceiling, and the standard per-MTok rates — cached input, uncached
/// input, output; off-peak halves every rate. The numbers move with
/// DeepSeek's price list (api-docs.deepseek.com/quick_start/pricing,
/// fetched 2026-10-10); the endpoint serves none of them.
struct DeepSeekFacts {
    id: &'static str,
    context: u64,
    output: u64,
    cached_input_per_mtok: f64,
    input_per_mtok: f64,
    output_per_mtok: f64,
    /// Whether the model accepts image input. DeepSeek's vision guide
    /// documents `deepseek-flash` as accepting images, while
    /// `deepseek-v4-pro` stays text-only; the generic `/models` listing
    /// carries no modality, so the catalog states the verdict.
    vision: bool,
}

const DEEPSEEK_CATALOG: &[DeepSeekFacts] = &[
    DeepSeekFacts {
        id: "deepseek-flash",
        // Accepts text and images, per DeepSeek's vision guide.
        vision: true,
        context: 1_048_576,
        output: 393_216,
        cached_input_per_mtok: 0.006,
        input_per_mtok: 0.30,
        output_per_mtok: 1.20,
    },
    DeepSeekFacts {
        id: "deepseek-v4-pro",
        // Text-only chat, per DeepSeek's model documentation.
        vision: false,
        context: 1_048_576,
        output: 393_216,
        cached_input_per_mtok: 0.044,
        input_per_mtok: 1.32,
        output_per_mtok: 3.96,
    },
];

/// Fills DeepSeek's published facts into a listing: ids naming a catalog
/// row gain their context window and pricing, everything else passes
/// through untouched — unknown models simply show no facts.
fn decorate_deepseek(models: &mut [ModelCard]) {
    for model in models {
        if let Some(row) = DEEPSEEK_CATALOG.iter().find(|row| row.id == model.id) {
            model.input_token_limit = Some(row.context);
            model.output_token_limit = Some(row.output);
            // Unknown ids stay `None`: a generic server exposes no
            // modality metadata, and no verdict beats a guessed one.
            model.supports_vision = Some(row.vision);
            model.pricing = Some(Pricing {
                input_per_mtok: row.input_per_mtok,
                output_per_mtok: row.output_per_mtok,
                cached_input_per_mtok: Some(row.cached_input_per_mtok),
            });
        }
    }
}

/// Attaches the bearer header when a key is configured; an empty key means
/// an anonymous request (local servers).
trait Authorized {
    fn authorized(
        self,
        api_key: &str,
        extra: &reqwest::header::HeaderMap,
    ) -> reqwest::RequestBuilder;
}

impl Authorized for reqwest::RequestBuilder {
    fn authorized(
        self,
        api_key: &str,
        extra: &reqwest::header::HeaderMap,
    ) -> reqwest::RequestBuilder {
        let built = if api_key.is_empty() {
            self
        } else {
            self.bearer_auth(api_key)
        };
        built.headers(extra.clone())
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`].
pub fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        message: format!(
            "openai-compatible api error (HTTP {}): {}",
            status.as_u16(),
            parsed.message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot be
/// carried in [`ProviderError`] directly (it is `Clone + Eq`).
pub fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(err.to_string())
    }
}

/// Parses one SSE payload as a chunk and folds it into the assembler. An
/// empty payload (keep-alive) is skipped.
/// Drains one chat-completions SSE response to its `[DONE]` sentinel,
/// assembling the output and emitting streaming events along the way.
/// The whole openai-dialect family completes through this: a provider
/// acquires its own response (copilot's token dance, openrouter's
/// routing headers) and hands it here.
pub async fn drain_chat_stream(
    response: reqwest::Response,
    dialect: &crate::wire::Dialect,
    sink: &mut dyn EventSink,
) -> Result<ModelOutput, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut assembler = ChunkAssembler::new(dialect);
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(network_error)?;
        for payload in decoder.push(&bytes) {
            if payload.trim() == DONE_SENTINEL {
                return Ok(assembler.finish());
            }
            feed_chunk(&payload, &mut assembler, sink)?;
            if let Some(err) = decoder.take_error() {
                return Err(ProviderError::Network(err));
            }
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
impl ModelListing for OpenAiCompatibleProvider {
    /// The text chat models the server can serve (`GET {base}/models` →
    /// `data[].id`, plus whatever metadata the entry carries). Sparse
    /// entries are the norm on generic servers; see
    /// [`crate::wire::normalize_models`].
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let response = self
            .http
            .get(format!("{}/models", self.base_url))
            .authorized(&self.api_key, &self.extra_headers)
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
        let page: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed models page: {err}"),
            status: None,
        })?;
        let mut models = crate::wire::normalize_models(&page);
        models.retain(is_text);
        if self.base_url.contains(DEEPSEEK_HOST) {
            decorate_deepseek(&mut models);
        }
        Ok(models)
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
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
        let body = crate::wire::request_body(&req, &self.dialect);
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .http
            .post(&url)
            .authorized(&self.api_key, &self.extra_headers)
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
        drain_chat_stream(response, &self.dialect, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::normalize_models;

    #[test]
    fn debug_prints_presence_never_the_key() {
        let provider = OpenAiCompatibleProvider::new("sk-live-secret");
        let printed = format!("{provider:?}");
        assert!(!printed.contains("sk-live-secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // The plain data stays visible.
        assert!(printed.contains("base_url"), "{printed}");
    }

    #[test]
    fn deepseek_models_carry_the_published_facts() {
        let page = serde_json::json!({
            "data": [
                { "id": "deepseek-flash" },
                { "id": "deepseek-v4-pro" },
                { "id": "some-local-model", "context_length": 8192 }
            ]
        });
        let mut models = normalize_models(&page);
        assert!(models.iter().all(|model| model.pricing.is_none()));
        assert!(models[2].input_token_limit == Some(8192));

        decorate_deepseek(&mut models);
        let flash = &models[0];
        assert_eq!(flash.input_token_limit, Some(1_048_576));
        assert_eq!(flash.output_token_limit, Some(393_216));
        // Flash accepts images; the pro row is text-only.
        assert_eq!(flash.supports_vision, Some(true));
        let pricing = flash.pricing.expect("flash is priced");
        assert_eq!(pricing.input_per_mtok, 0.30);
        assert_eq!(pricing.output_per_mtok, 1.20);
        assert_eq!(pricing.cached_input_per_mtok, Some(0.006));

        let pro = &models[1];
        assert_eq!(pro.pricing.expect("pro is priced").input_per_mtok, 1.32);
        assert_eq!(pro.supports_vision, Some(false));

        // The server's own facts win: a decorated listing leaves unknown
        // models exactly as the endpoint described them — modality
        // included, since the generic listing carries none.
        assert_eq!(models[2].input_token_limit, Some(8192));
        assert!(models[2].pricing.is_none());
        assert_eq!(models[2].supports_vision, None);
    }
}
