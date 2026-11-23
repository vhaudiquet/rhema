//! The [`OpenAiCompatibleProvider`]: HTTP and SSE transport for any
//! OpenAI-compatible chat-completions API.
//!
//! The base URL is the full versioned base, e.g. `https://api.openai.com/v1`
//! or a local server's `/v1`, and is used verbatim: the endpoints are
//! `{base}/chat/completions` and `{base}/models`, with nothing appended.
//! Trailing slashes are trimmed, so either spelling of a base works.
//!
//! Auth is by API key in the `Authorization: Bearer` header, never a query
//! parameter, so the key never lands in URLs or access logs. An empty key
//! omits the header entirely, for local servers that need no auth.
//! Transport failures map onto [`ProviderError`]: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Other`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`], unparseable
//! stream chunks and mid-stream error chunks to [`ProviderError::Api`] too.
//!
//! Timeouts are layered: the client bounds connection setup (10s) and each
//! stream read (90s, a stall guard, not a duration cap, because a long
//! generation is legitimate), and non-streaming calls (the models list)
//! also carry a 20s overall request timeout. Streaming calls get no
//! total timeout for the same reason.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{Allowance, ModelCard, ModelListing, Pricing, SseDecoder};

use crate::responses::{ResponsesAssembler, WireStreamEvent};
use crate::wire::{ChatCompletionChunk, ChunkAssembler, WireBalance, WireErrorBody, is_text};

/// The production OpenAI API base, version segment included.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The SSE payload that ends every OpenAI-compatible chat stream.
pub const DONE_SENTINEL: &str = "[DONE]";

/// The HTTP client every instance shares: bounded connects and name
/// lookups, stream reads stall-guarded, never total-time capped.
pub fn http_client() -> reqwest::Client {
    rhema_provider::http::client_builder()
        .build()
        .expect("the shared client builder is valid")
}

/// DeepSeek's OpenAI-compatible endpoint, the base the `deepseek`
/// kind speaks by default, over the generic adapter.
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// Which request shape a provider speaks. Both ride the same base URL,
/// key, headers, and client; only the endpoint, the body, and the
/// stream vocabulary differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Api {
    /// `POST {base}/chat/completions`, the default, the dialect every
    /// OpenAI-compatible server speaks.
    #[default]
    Chat,
    /// `POST {base}/responses`, the Responses API, spoken by OpenAI,
    /// DeepSeek, OpenCode's gateway, and llama.cpp's server. A tool
    /// result's images ride its `function_call_output` as content parts
    /// there, with no follow-up turn.
    Responses,
}

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
    /// Which request shape `complete` speaks; the default is chat
    /// completions.
    api: Api,
    /// Extra headers every request carries, routing metadata,
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
            api: Api::Chat,
            extra_headers: reqwest::header::HeaderMap::new(),
        }
    }

    /// Targets `base_url`, the FULL versioned base (e.g.
    /// `https://api.openai.com/v1`, `http://localhost:8080/v1`), used
    /// verbatim with the endpoint paths appended; trailing slashes are
    /// trimmed. This is the constructor for every compatible server that is
    /// not OpenAI itself.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Speaks another [`Dialect`](crate::Dialect), another server family's spelling of
    /// the same chat-completions vocabulary. This is how
    /// `rhema-provider-openrouter` and `…-llama-cpp` reuse this
    /// transport: their providers hold one of these, dialed to their
    /// dialect.
    pub fn with_dialect(mut self, dialect: crate::wire::Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Speaks the Responses API instead of chat completions, same base
    /// URL, same key; the endpoint, the body, and the stream vocabulary
    /// change. Address a server that serves `{base}/responses` (OpenAI,
    /// DeepSeek, OpenCode's gateway, llama.cpp's server).
    pub fn with_api(mut self, api: Api) -> Self {
        self.api = api;
        self
    }

    /// Attaches headers every request carries. Called repeatedly, the
    /// maps merge; later values win per header name.
    pub fn with_extra_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.extra_headers.extend(headers);
        self
    }

    /// Posts a streaming request and hands back the live response: the
    /// key and extra headers attached, non-2xx mapped onto
    /// [`ProviderError::Api`]. No overall timeout, a long generation
    /// is legitimate, and the client's read timeout already bounds
    /// stalls between chunks.
    async fn post_stream<T: serde::Serialize>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .http
            .post(url)
            .authorized(&self.api_key, &self.extra_headers)
            .json(body)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(response_error(status, &headers, &text));
        }
        Ok(response)
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
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(response_error(status, &headers, &text));
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

/// The api.deepseek.com host, the one endpoint this adapter decorates
/// with a published catalog, because their `/models` serves ids alone.
const DEEPSEEK_HOST: &str = "api.deepseek.com";

/// One row of DeepSeek's published catalog: context window, output
/// ceiling, and the standard per-MTok rates, cached input, uncached
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
/// through untouched, unknown models simply show no facts.
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

/// Maps a non-2xx response onto [`ProviderError`]: [`api_error`], except
/// a 429 that names its reset becomes [`ProviderError::RateLimited`], so
/// the retry ladder can wait out a per-minute window instead of failing
/// the turn.
pub fn response_error(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    raw: &str,
) -> ProviderError {
    let err = api_error(status, raw);
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return err;
    }
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let body: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    match rate_limit_wait(header, &body.message(raw)) {
        Some(retry_after) => ProviderError::RateLimited {
            message: err.to_string(),
            retry_after,
        },
        None => err,
    }
}

/// The wait a 429 names, read from the most precise source first:
/// `retry-after-ms`, `retry-after` in seconds (an HTTP date is not
/// read), OpenAI's "try again in 2.004s" sentence, where the wait is a
/// Go-style duration or a spelled-out unit ("2 minutes"), then the
/// `x-ratelimit-reset-*` headers, the token window before the request
/// window since TPM is the limit long agent turns hit.
fn rate_limit_wait<'a>(
    header: impl Fn(&str) -> Option<&'a str>,
    message: &str,
) -> Option<Duration> {
    if let Some(ms) = header("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return seconds(ms / 1000.0);
    }
    if let Some(secs) = header("retry-after").and_then(|v| v.trim().parse::<f64>().ok()) {
        return seconds(secs);
    }
    let lower = message.to_ascii_lowercase();
    if let Some(at) = lower.find("try again in ") {
        let rest = &lower[at + "try again in ".len()..];
        let mut words = rest
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|word| !word.is_empty());
        // The sentence can end on the token: "in 2.004s." carries its
        // period on it.
        let token = words.next().unwrap_or("").trim_end_matches('.');
        // A bare number names no wait alone: its unit rides the word
        // after it ("2 minutes"). A Go-style token carries its own.
        if let Ok(bare) = token.parse::<f64>() {
            if let Some(scale) = words
                .next()
                .map(|word| word.trim_end_matches('.'))
                .and_then(unit_scale)
            {
                return seconds(bare * scale);
            }
        } else if let Some(wait) = go_duration(token) {
            return Some(wait);
        }
    }
    ["x-ratelimit-reset-tokens", "x-ratelimit-reset-requests"]
        .into_iter()
        .find_map(|name| header(name).and_then(|v| go_duration(v.trim())))
}

/// A non-negative, finite number of seconds as a [`Duration`]. A value
/// no duration can hold, an absurd header or an overflowed unit
/// product, reads as no wait: a rate limit that cannot be metered is
/// not one to sleep on.
fn seconds(secs: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(secs).ok()
}

/// A Go-style duration as OpenAI spells resets: `1h2m3.5s`, `6m0s`,
/// `120ms`, `2.004s`. A bare number reads as seconds.
fn go_duration(text: &str) -> Option<Duration> {
    if text.is_empty() {
        return None;
    }
    if let Ok(secs) = text.parse::<f64>() {
        return seconds(secs);
    }
    let mut total = 0.0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let value: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total += value * scale;
        rest = &rest[unit_len..];
    }
    seconds(total)
}

/// The second scale of a unit spelled out in a 429's sentence:
/// "2 minutes", "1 hour".
fn unit_scale(word: &str) -> Option<f64> {
    match word {
        "second" | "seconds" => Some(1.0),
        "minute" | "minutes" => Some(60.0),
        "hour" | "hours" => Some(3600.0),
        _ => None,
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
/// empty payload (keep-alive) is skipped. Mistral's SDK-shaped
/// `{"data": {...}}` event wrapper is unwrapped before deserialization.
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
        let payloads = decoder.push(&bytes);
        if let Some(err) = decoder.take_error() {
            return Err(ProviderError::Network(err));
        }
        for payload in payloads {
            if payload.trim() == DONE_SENTINEL {
                return Ok(assembler.finish());
            }
            feed_chunk(&payload, &mut assembler, sink)?;
        }
    }
    let payloads = decoder.finish();
    if let Some(err) = decoder.take_error() {
        return Err(ProviderError::Network(err));
    }
    for payload in payloads {
        if payload.trim() == DONE_SENTINEL {
            break;
        }
        feed_chunk(&payload, &mut assembler, sink)?;
    }
    // The terminal chunk (a finish_reason, then the sentinel) is this
    // wire's proof the exchange ended on the server's own terms: the
    // bytes running out before either is a truncated stream, and
    // standing it up as a clean stop would commit the half answer.
    // The retry ladder sees a retryable network failure instead.
    if !assembler.has_finish_reason() {
        return Err(ProviderError::Network(
            "the chat stream ended without its finish_reason".to_owned(),
        ));
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
    // Mistral's SDK-shaped stream wraps each chunk in a `{"data": ...}`
    // event; most OpenAI-compatible servers send the chunk directly.
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|err| ProviderError::Api {
            message: format!("malformed SSE chunk: {err}"),
            status: None,
        })?;
    let chunk: ChatCompletionChunk = if let Some(data) = value
        .get("data")
        .filter(|data| data.is_object() && value.get("choices").is_none())
    {
        serde_json::from_value(data.clone())
    } else {
        serde_json::from_value(value)
    }
    .map_err(|err| ProviderError::Api {
        message: format!("malformed SSE chunk: {err}"),
        status: None,
    })?;
    assembler.push(chunk, sink)
}

/// Drains one Responses-API SSE stream to its terminal event
/// (`response.completed` / `response.incomplete` / `response.failed`),
/// assembling the output and emitting streaming events along the way.
/// This wire has no `[DONE]` sentinel; one arriving from a proxy ends
/// the stream early, with whatever has assembled standing. The bytes
/// running out without a terminal event is a truncated stream: the
/// drain fails as a retryable network error, never a silent stop.
pub async fn drain_responses_stream(
    response: reqwest::Response,
    sink: &mut dyn EventSink,
) -> Result<ModelOutput, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut assembler = ResponsesAssembler::new();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(network_error)?;
        let payloads = decoder.push(&bytes);
        if let Some(err) = decoder.take_error() {
            return Err(ProviderError::Network(err));
        }
        for payload in payloads {
            if payload.trim() == DONE_SENTINEL {
                return Ok(assembler.finish());
            }
            feed_event(&payload, &mut assembler, sink)?;
        }
    }
    let payloads = decoder.finish();
    if let Some(err) = decoder.take_error() {
        return Err(ProviderError::Network(err));
    }
    for payload in payloads {
        if payload.trim() == DONE_SENTINEL {
            break;
        }
        feed_event(&payload, &mut assembler, sink)?;
    }
    // The terminal event is this wire's proof the exchange ended on
    // the server's own terms: the bytes running out without one is a
    // truncated stream, and standing it up as a clean stop would
    // commit the half answer. The retry ladder sees a retryable
    // network failure and re-issues instead.
    if !assembler.was_completed() {
        return Err(ProviderError::Network(
            "the responses stream ended without its terminal event".to_owned(),
        ));
    }
    Ok(assembler.finish())
}

fn feed_event(
    payload: &str,
    assembler: &mut ResponsesAssembler,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderError> {
    if payload.trim().is_empty() {
        return Ok(());
    }
    let event: WireStreamEvent =
        serde_json::from_str(payload).map_err(|err| ProviderError::Api {
            message: format!("malformed SSE event: {err}"),
            status: None,
        })?;
    assembler.push(event, sink)
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
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(response_error(status, &headers, &text));
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
        let req = req.normalized();
        match self.api {
            Api::Chat => {
                let body = crate::wire::request_body(&req, &self.dialect);
                let url = format!("{}/chat/completions", self.base_url);
                let response = self.post_stream(&url, &body).await?;
                drain_chat_stream(response, &self.dialect, sink).await
            }
            Api::Responses => {
                let body = crate::responses::request_body(&req, &self.dialect);
                let url = format!("{}/responses", self.base_url);
                let response = self.post_stream(&url, &body).await?;
                drain_responses_stream(response, sink).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::normalize_models;

    fn wait(headers: &[(&str, &str)], message: &str) -> Option<Duration> {
        rate_limit_wait(
            |name| {
                headers
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| *value)
            },
            message,
        )
    }

    #[test]
    fn go_durations_read_as_openai_spells_them() {
        assert_eq!(go_duration("2.004s"), Some(Duration::from_millis(2004)));
        assert_eq!(go_duration("120ms"), Some(Duration::from_millis(120)));
        assert_eq!(go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(go_duration("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(go_duration("7"), Some(Duration::from_secs(7)));
        assert_eq!(go_duration(""), None);
        assert_eq!(go_duration("soon"), None);
    }

    #[test]
    fn a_prose_wait_reads_spelled_out_units() {
        assert_eq!(
            wait(&[], "slow down, please try again in 1 minute."),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            wait(
                &[],
                "Please try again in 2 minutes. Visit https://example.com."
            ),
            Some(Duration::from_secs(120))
        );
        // The unit word after the number names the scale, so the
        // seconds case reads the same as before.
        assert_eq!(
            wait(&[], "Please try again in 30 seconds."),
            Some(Duration::from_secs(30))
        );
        // A unit the scale table does not know, and a bare number
        // with no unit word, name no wait.
        assert_eq!(wait(&[], "Please try again in 2 days."), None);
        assert_eq!(wait(&[], "Please try again."), None);
    }

    #[test]
    fn an_absurd_reset_names_no_wait() {
        // `Duration::from_secs_f64` panicked on these, so one hostile
        // header took the turn down; a wait that cannot be metered is
        // not one to sleep on.
        assert_eq!(wait(&[("retry-after", "1e300")], "rate limited"), None);
        assert_eq!(
            wait(&[("x-ratelimit-reset-tokens", "1e400s")], "rate limited"),
            None
        );
    }

    #[test]
    fn a_rate_limit_wait_prefers_the_most_precise_source() {
        assert_eq!(
            wait(&[("retry-after-ms", "1500"), ("retry-after", "2")], ""),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            wait(&[("retry-after", "20")], ""),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            wait(
                &[("x-ratelimit-reset-tokens", "30s")],
                "Rate limit reached on tokens per min (TPM): Limit 30000, \
                 Used 29000, Requested 2000. Please try again in 2.004s. Visit \
                 https://platform.openai.com/account/rate-limits to learn more."
            ),
            Some(Duration::from_millis(2004))
        );
        assert_eq!(
            wait(
                &[
                    ("x-ratelimit-reset-requests", "1s"),
                    ("x-ratelimit-reset-tokens", "8.5s")
                ],
                "rate limited"
            ),
            Some(Duration::from_millis(8500))
        );
        // An HTTP-date retry-after is not read, and nothing else names a wait.
        assert_eq!(
            wait(
                &[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")],
                "slow down"
            ),
            None
        );
    }

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
        // models exactly as the endpoint described them, modality
        // included, since the generic listing carries none.
        assert_eq!(models[2].input_token_limit, Some(8192));
        assert!(models[2].pricing.is_none());
        assert_eq!(models[2].supports_vision, None);
    }
}
