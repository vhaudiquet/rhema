//! The [`GoogleProvider`]: HTTP and SSE transport for the Gemini
//! Generative Language API.
//!
//! Auth is by API key sent in the `x-goog-api-key` header, never a query
//! parameter, so the key never lands in URLs or access logs. Only streaming
//! (`:streamGenerateContent?alt=sse`) is spoken; every call streams and
//! assembles. Transport failures map onto
//! [`ProviderError`]: timeouts to [`ProviderError::Timeout`], other network
//! failures to [`ProviderError::Network`], non-2xx responses (with the
//! body's `error.message` when present) to [`ProviderError::Api`] with
//! the HTTP status, except a 429 that names its reset, which becomes
//! [`ProviderError::RateLimited`] so the retry ladder can wait out a
//! per-minute window instead of failing the turn, and unparseable
//! stream chunks to [`ProviderError::Api`] without one, as a protocol
//! violation. A stream can also die mid-turn:
//! Gemini then sends a normal chunk-shaped `{"error": ...}` payload, which
//! fails the same way, with the body's `error.message` when present.
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
use rhema_provider::{ModelCard, ModelListing, SseDecoder};

use crate::wire::{
    ChunkAssembler, GenerateContentResponse, WireErrorBody, WireModelsPage, is_text,
};

/// The production Generative Language API base (v1beta).
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The HTTP client every instance shares: bounded connects and name
/// lookups, stream reads stall-guarded, never total-time capped.
fn http_client() -> reqwest::Client {
    rhema_provider::http::client_builder()
        .build()
        .expect("the shared client builder is valid")
}

/// Google Gemini model backend, speaking the Generative Language REST API.
pub struct GoogleProvider {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never prints; the plain data does.
        f.debug_struct("GoogleProvider")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("http", &self.http)
            .finish()
    }
}

impl GoogleProvider {
    /// A provider authenticating with `api_key` against
    /// [`DEFAULT_BASE_URL`].
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            http: http_client(),
        }
    }

    /// Overrides the base URL (trailing slashes trimmed), for tests against
    /// a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot
/// be carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(err.to_string())
    }
}

/// The `retry-after` header's value, when the response carries one.
/// Read before the body is taken, which consumes the response, so the
/// value is copied out first.
fn retry_after_header(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Maps a non-2xx response onto [`ProviderError`]: an api error with
/// the body's message and status, except a 429 that names its reset
/// becomes [`ProviderError::RateLimited`], so the retry ladder can wait
/// out a per-minute window instead of failing the turn. Two sources
/// name the reset, the `RetryInfo` detail the error body folds its
/// quota facts into (`retryDelay`, a `30s`-style string) and the
/// `retry-after` header in seconds; the body's own clock is the
/// specific quota's wait, so it wins when both are present.
fn response_error(
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
    body: &WireErrorBody,
    raw: &str,
) -> ProviderError {
    let err = ProviderError::Api {
        message: format!(
            "gemini api error (HTTP {}): {}",
            status.as_u16(),
            body.message(raw)
        ),
        status: Some(status.as_u16()),
    };
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return err;
    }
    let wait = body
        .retry_delay()
        .and_then(retry_delay)
        .or_else(|| retry_after.and_then(header_seconds));
    match wait {
        Some(retry_after) => ProviderError::RateLimited {
            message: err.to_string(),
            retry_after,
        },
        None => err,
    }
}

/// A `RetryInfo` `retryDelay` string (`"30s"`, `"5.5s"`, a proto
/// duration spelling that never carries a minutes or hours unit) as the
/// wait it names. A value no duration can hold, negative or absurd,
/// reads as no wait: a rate limit that cannot be metered is not one to
/// sleep on.
fn retry_delay(raw: &str) -> Option<Duration> {
    let secs = raw.trim().strip_suffix('s')?;
    Duration::try_from_secs_f64(secs.trim().parse().ok()?).ok()
}

/// A `retry-after` header value in decimal seconds (an HTTP date is
/// not read) as the wait it names; a value no duration can hold reads
/// as no wait.
fn header_seconds(raw: &str) -> Option<Duration> {
    Duration::try_from_secs_f64(raw.trim().parse().ok()?).ok()
}

/// Parses one SSE payload as a response chunk and folds it into the
/// assembler. An empty payload (keep-alive) is skipped.
fn feed_chunk(
    payload: &str,
    assembler: &mut ChunkAssembler,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderError> {
    if payload.trim().is_empty() {
        return Ok(());
    }
    let chunk: GenerateContentResponse =
        serde_json::from_str(payload).map_err(|err| ProviderError::Api {
            message: format!("malformed SSE chunk: {err}"),
            status: None,
        })?;
    assembler.push(chunk, sink)
}

#[async_trait]
impl ModelListing for GoogleProvider {
    /// Every text chat model the key can see, following pagination until
    /// the list is exhausted. The ListModels API also reports embeddings,
    /// video, speech, and image models, those cannot drive a coding agent
    /// and are filtered here, by Gemini-catalog knowledge that never rides
    /// on the shared [`ModelCard`]. `models/` is stripped from
    /// `ModelCard::id`, and the curated table prices each row the listing
    /// itself leaves unpriced.
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let mut models = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}/models?pageSize=1000", self.base_url);
            if let Some(token) = &page_token {
                url.push_str(&format!("&pageToken={token}"));
            }
            let response = self
                .http
                .get(&url)
                .header("x-goog-api-key", &self.api_key)
                // The one overall timeout: a bounded listing call, not a
                // stream of unbounded length.
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .map_err(network_error)?;
            let status = response.status();
            if !status.is_success() {
                let retry_after = retry_after_header(&response);
                let text = response.text().await.unwrap_or_default();
                let parsed: WireErrorBody = serde_json::from_str(&text).unwrap_or_default();
                return Err(response_error(
                    status,
                    retry_after.as_deref(),
                    &parsed,
                    &text,
                ));
            }
            let page: WireModelsPage = response.json().await.map_err(|err| ProviderError::Api {
                message: format!("malformed models page: {err}"),
                status: None,
            })?;
            // The row count before the models move out of the page.
            let fetched = page.models.len();
            models.extend(
                page.models
                    .into_iter()
                    .map(ModelCard::from)
                    // A listing-carried price would win; ListModels has
                    // none, so the curated table decides.
                    .map(|mut card| {
                        card.pricing = card.pricing.or_else(|| crate::model_pricing(&card.id));
                        card
                    })
                    .filter(|card| card.supports_chat() && is_text(card)),
            );
            // The zero-row guard keeps a misbehaving endpoint that keeps
            // handing out a next token over empty pages from spinning this
            // loop forever.
            if fetched == 0 || page.next_page_token.is_none() {
                return Ok(models);
            }
            page_token = page.next_page_token;
        }
    }
}

#[async_trait]
impl ModelProvider for GoogleProvider {
    /// # Errors
    ///
    /// Fails with [`ProviderError::InvalidRequest`] when the request
    /// cannot map onto the wire, [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx response (a 429 that names its reset is
    /// [`ProviderError::RateLimited`] instead), and [`ProviderError::Api`] without
    /// one on a malformed stream chunk or a mid-stream error chunk, and
    /// [`ProviderError::Network`] when the stream closes before its
    /// terminal chunk.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let req = req.normalized();
        let body = crate::wire::request_body(&req)?;
        let model = req.params.model.trim();
        // `models/` is the wire prefix; accept both spellings.
        let model = model.strip_prefix("models/").unwrap_or(model);
        let url = format!(
            "{}/models/{model}:streamGenerateContent?alt=sse",
            self.base_url
        );

        let response = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after_header(&response);
            let text = response.text().await.unwrap_or_default();
            let parsed: WireErrorBody = serde_json::from_str(&text).unwrap_or_default();
            return Err(response_error(
                status,
                retry_after.as_deref(),
                &parsed,
                &text,
            ));
        }

        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut assembler = ChunkAssembler::new();
        while let Some(item) = stream.next().await {
            let bytes = item.map_err(network_error)?;
            let payloads = decoder.push(&bytes);
            if let Some(err) = decoder.take_error() {
                return Err(ProviderError::Network(err));
            }
            for payload in payloads {
                feed_chunk(&payload, &mut assembler, sink)?;
            }
        }
        let payloads = decoder.finish();
        if let Some(err) = decoder.take_error() {
            return Err(ProviderError::Network(err));
        }
        for payload in payloads {
            feed_chunk(&payload, &mut assembler, sink)?;
        }
        // The final chunk's `finishReason` (or a refused prompt's
        // `blockReason`) is this wire's proof the exchange ended on the
        // server's own terms: the bytes running out before one is a
        // truncated stream, and standing it up as a clean stop would
        // commit the half answer. The retry ladder sees a retryable
        // network failure and re-issues instead.
        if !assembler.saw_terminal() {
            return Err(ProviderError::Network(
                "the gemini stream ended without its finishReason".to_owned(),
            ));
        }
        Ok(assembler.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_prints_presence_never_the_key() {
        let provider = GoogleProvider::new("AIza-secret-key");
        let printed = format!("{provider:?}");
        assert!(!printed.contains("AIza-secret-key"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // The plain data stays visible.
        assert!(printed.contains("base_url"), "{printed}");
    }
}
