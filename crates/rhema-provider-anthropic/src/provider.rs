//! The [`AnthropicProvider`]: HTTP and SSE transport for the Anthropic
//! Messages API.
//!
//! Auth is by API key in the `x-api-key` header — never a query parameter,
//! so the key never lands in URLs or access logs — with the pinned
//! `anthropic-version` header ([`ANTHROPIC_VERSION`]). Anthropic-compatible
//! endpoints that authenticate by bearer token instead (Z.ai's GLM Coding
//! Plan route, [`ZAI_BASE_URL`]) switch the header scheme with
//! `with_bearer_auth`; the dialect is unchanged. Only the streaming
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
use rhema_provider::{ModelCard, Pricing, SseDecoder};

use crate::wire::{ChunkAssembler, StreamEvent, WireErrorBody, WireModelsPage};

/// The production Anthropic API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The API version pin every request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The Z.ai GLM Coding Plan's Anthropic-compatible base — the
/// `with_base_url` target for that backend, which also switches the
/// header scheme with `with_bearer_auth` (Z.ai documents token auth, not
/// `x-api-key`).
pub const ZAI_BASE_URL: &str = "https://api.z.ai/api/anthropic";

/// The `User-Agent` every request carries: honest client identification,
/// so the server side sees rhema as rhema — never another tool's mark.
pub const USER_AGENT: &str = concat!("rhema/", env!("CARGO_PKG_VERSION"));

/// The largest page size the models endpoint honors.
const MODELS_PAGE_LIMIT: u32 = 1000;

/// Anthropic backend, speaking the Messages API.
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    /// Bearer instead of `x-api-key`, per `with_bearer_auth`.
    bearer_auth: bool,
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
            .user_agent(USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            bearer_auth: false,
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for the
    /// Anthropic-compatible endpoints and for tests against a mock
    /// server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Authenticates with `Authorization: Bearer <key>` instead of the
    /// default `x-api-key` — the scheme the Anthropic-compatible
    /// endpoints document (Z.ai's GLM Coding Plan route,
    /// [`ZAI_BASE_URL`]). Everything else about the dialect is unchanged.
    pub fn with_bearer_auth(mut self) -> Self {
        self.bearer_auth = true;
        self
    }

    /// Every model the key can see, following `has_more`/`after_id`
    /// pagination until the list is exhausted. Anthropic's listing is
    /// text chat models only, so there is nothing to filter; the
    /// api.z.ai route's rows gain the published catalog facts
    /// ([`decorate_zai`]).
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
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
            models.extend(page.data.into_iter().map(ModelCard::from));
            // A missing cursor or a zero-row page keeps a misbehaving mirror
            // from spinning this loop forever.
            match page.has_more.then_some(page.last_id).flatten() {
                Some(cursor) if fetched > 0 => after_id = Some(cursor),
                _ => {
                    if self.base_url.contains(ZAI_HOST) {
                        decorate_zai(&mut models);
                    }
                    return Ok(models);
                }
            }
        }
    }

    /// Common headers of every request: the key by the configured scheme
    /// and the pinned API version.
    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = if self.bearer_auth {
            request.header("Authorization", format!("Bearer {}", self.api_key))
        } else {
            request.header("x-api-key", &self.api_key)
        };
        request.header("anthropic-version", ANTHROPIC_VERSION)
    }
}

/// The api.z.ai host — the one endpoint this adapter decorates, because
/// its `/models` serves ids alone: the Anthropic schema has no room for
/// catalog facts.
const ZAI_HOST: &str = "api.z.ai";

/// One row of Z.ai's published GLM catalog: context window, output
/// ceiling, and the standard per-MTok rates — cached input, uncached
/// input, output. The numbers move with Z.ai's price list
/// (docs.z.ai/guides/overview/pricing and the models' pages, fetched
/// 2026-10-14); the endpoint serves none of them. List prices, even on
/// the Coding Plan, which bills subscription credits rather than tokens.
struct ZaiFacts {
    id: &'static str,
    context: u64,
    output: u64,
    cached_input_per_mtok: f64,
    input_per_mtok: f64,
    output_per_mtok: f64,
}

const ZAI_CATALOG: &[ZaiFacts] = &[
    ZaiFacts {
        id: "glm-5.3",
        context: 1_000_000,
        output: 128_000,
        cached_input_per_mtok: 0.26,
        input_per_mtok: 1.4,
        output_per_mtok: 4.4,
    },
    ZaiFacts {
        id: "glm-5.3-flash",
        context: 1_000_000,
        output: 128_000,
        cached_input_per_mtok: 0.03,
        input_per_mtok: 0.15,
        output_per_mtok: 0.5,
    },
    ZaiFacts {
        id: "glm-5.3-flashx",
        context: 1_000_000,
        output: 128_000,
        cached_input_per_mtok: 0.075,
        input_per_mtok: 0.37,
        output_per_mtok: 1.25,
    },
];

/// Fills Z.ai's published facts into a listing: ids naming a catalog row
/// gain their context window and pricing, everything else passes through
/// untouched — unknown models simply show no facts. The match is
/// case-insensitive: the endpoint spells the ids lowercase, Z.ai's own
/// docs uppercase.
fn decorate_zai(models: &mut [ModelCard]) {
    for model in models {
        if let Some(row) = ZAI_CATALOG
            .iter()
            .find(|row| row.id.eq_ignore_ascii_case(&model.id))
        {
            model.input_token_limit = Some(row.context);
            model.output_token_limit = Some(row.output);
            model.pricing = Some(Pricing {
                input_per_mtok: row.input_per_mtok,
                output_per_mtok: row.output_per_mtok,
                cached_input_per_mtok: Some(row.cached_input_per_mtok),
            });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> ModelCard {
        ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }

    #[test]
    fn zai_rows_carry_the_published_facts_and_unknown_pass_through() {
        let mut models = vec![row("glm-5.3"), row("GLM-5.3-FLASHX"), row("glm-brand-new")];
        decorate_zai(&mut models);
        assert_eq!(models[0].input_token_limit, Some(1_000_000));
        assert_eq!(models[0].output_token_limit, Some(128_000));
        assert_eq!(
            models[0].pricing,
            Some(Pricing {
                input_per_mtok: 1.4,
                output_per_mtok: 4.4,
                cached_input_per_mtok: Some(0.26),
            })
        );
        // The endpoint spells the ids lowercase, the docs uppercase —
        // both hit the catalog row.
        assert_eq!(models[1].input_token_limit, Some(1_000_000));
        assert_eq!(
            models[1].pricing.map(|pricing| pricing.input_per_mtok),
            Some(0.37)
        );
        // An id outside the catalog shows no invented facts.
        assert_eq!(models[2].input_token_limit, None);
        assert_eq!(models[2].pricing, None);
    }
}
