//! The [`AnthropicProvider`]: the Anthropic Messages API over the
//! [`Transport`] seam.
//!
//! Auth is by API key in the `x-api-key` header — never a query parameter,
//! so the key never lands in URLs or access logs — with the pinned
//! `anthropic-version` header ([`ANTHROPIC_VERSION`]). Anthropic-compatible
//! endpoints that authenticate by bearer token instead (Z.ai's GLM Coding
//! Plan route, [`ZAI_BASE_URL`]) switch the header scheme with
//! `with_bearer_auth`; the dialect is unchanged. Only the streaming
//! dialect is spoken; every call streams and assembles.
//!
//! The HTTP ride is injectable: the default constructor speaks reqwest
//! (feature `native`), and [`AnthropicProvider::new_with_transport`]
//! accepts any [`Transport`] — the wasm frontends bring one over host
//! fetch, whose `!Send` JS stream is bridged inside the transport so the
//! seam stays `Send`. Transport failures map onto [`ProviderError`]:
//! timeouts to [`ProviderError::Timeout`], everything else to
//! `network error: ...` ([`ProviderError::Network`], retryable), non-2xx
//! responses (with the body's `error.message` when present) to
//! [`ProviderError::Api`] carrying the HTTP status structurally, and
//! unparseable stream chunks and mid-stream error events to
//! [`ProviderError::Api`] without one.
//!
//! Timeouts are layered where the transport allows: the native client
//! bounds connection setup (10s) and each stream read (90s — a stall
//! guard, not a duration cap, because a long generation is legitimate),
//! and non-streaming calls (the models list, the quota probe) carry a
//! 20s overall budget on their request. Streaming calls get no total
//! timeout for the same reason.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{
    Allowance, HttpRequest, HttpResponse, ModelCard, Pricing, SseDecoder, Transport, TransportError,
};

use crate::wire::{ChunkAssembler, StreamEvent, WireErrorBody, WireModelsPage, WireZaiQuota};

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

/// The overall budget of the bounded, listing-style calls: the models
/// list and the quota probe. Not a stream's budget — a generation runs
/// unbounded.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// Anthropic backend, speaking the Messages API.
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    /// Bearer instead of `x-api-key`, per `with_bearer_auth`.
    bearer_auth: bool,
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport is a dyn seam: it prints as a placeholder, the
        // plain data prints in full. The key never prints.
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("bearer_auth", &self.bearer_auth)
            .field("transport", &"<dyn Transport>")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AnthropicProvider {
    /// A provider authenticating with `api_key` against
    /// [`DEFAULT_BASE_URL`], over the native reqwest transport.
    #[cfg(feature = "native")]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::new_with_transport(api_key, Arc::new(crate::transport::ReqwestTransport::new()))
    }

    /// A provider over a caller-supplied transport — the seam the wasm
    /// frontends come in through. Any [`Transport`] fits; the provider
    /// never sees what the bytes ride on.
    pub fn new_with_transport(api_key: impl Into<String>, transport: Arc<dyn Transport>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            bearer_auth: false,
            transport,
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
            // The query is assembled by hand: the only parameter that
            // varies is a cursor the same endpoint handed out (an id of
            // letters, digits, and dashes), so there is nothing to
            // percent-encode.
            let mut url = format!("{}/v1/models?limit={MODELS_PAGE_LIMIT}", self.base_url);
            if let Some(after) = &after_id {
                url.push_str("&after_id=");
                url.push_str(after);
            }
            let mut response = self
                .transport
                .send(
                    self.authenticated(HttpRequest::new("GET", url))
                        .with_timeout(LIST_TIMEOUT),
                )
                .await
                .map_err(transport_error)?;
            if !is_success(response.status) {
                let text = read_all(&mut response).await?;
                return Err(api_error(
                    response.status,
                    &String::from_utf8_lossy(&text),
                    &response.headers,
                ));
            }
            let body = read_all(&mut response).await?;
            let page: WireModelsPage =
                serde_json::from_slice(&body).map_err(|err| ProviderError::Api {
                    message: format!("malformed models page: {err}"),
                    status: None,
                })?;
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

    /// The subscription allowance the base URL's platform reports: the
    /// GLM Coding Plan's metered windows — the 5-hour and weekly shares
    /// its badge shows — on the Z.ai hosts, `Ok(None)` anywhere else.
    /// Anthropic itself has no such endpoint, and a proxy in front of the
    /// route serves no Z.ai monitor API either, so both degrade to no
    /// badge instead of a per-turn error.
    pub async fn allowance(&self) -> Result<Option<Allowance>, ProviderError> {
        let Some(url) = zai_quota_url(&self.base_url) else {
            return Ok(None);
        };
        self.fetch_zai_quota(&url).await.map(Some)
    }

    /// The monitor endpoint's call, factored off [`Self::allowance`] so
    /// the offline tests can mount it without pointing the provider at a
    /// Z.ai host.
    async fn fetch_zai_quota(&self, url: &str) -> Result<Allowance, ProviderError> {
        // Bearer unconditionally: the monitor endpoint is a Z.ai platform
        // API, and Z.ai documents token auth, not `x-api-key`.
        let mut response = self
            .transport
            .send(
                HttpRequest::new("GET", url.to_owned())
                    .with_header("Authorization", format!("Bearer {}", self.api_key))
                    .with_timeout(LIST_TIMEOUT),
            )
            .await
            .map_err(transport_error)?;
        let status = response.status;
        let text = read_all(&mut response).await?;
        if !is_success(status) {
            return Err(ProviderError::Api {
                message: format!(
                    "zai quota error (HTTP {}): {}",
                    status,
                    String::from_utf8_lossy(&text)
                ),
                status: Some(status),
            });
        }
        let quota: WireZaiQuota =
            serde_json::from_slice(&text).map_err(|err| ProviderError::Api {
                message: format!("malformed quota response: {err}"),
                status: None,
            })?;
        if !quota.success {
            // A rejected token rides HTTP 200; the envelope's own fields
            // carry the failure.
            return Err(ProviderError::Api {
                message: match (quota.code, quota.msg) {
                    (Some(code), Some(msg)) => format!("zai quota error (code {code}): {msg}"),
                    (Some(code), None) => format!(
                        "zai quota error (code {code}): {text}",
                        text = String::from_utf8_lossy(&text)
                    ),
                    _ => format!(
                        "zai quota error: {text}",
                        text = String::from_utf8_lossy(&text)
                    ),
                },
                // A rejected token answers HTTP 200: the envelope's code
                // is the failure, not a transport status.
                status: None,
            });
        }
        crate::wire::allowance(&quota).ok_or_else(|| ProviderError::Api {
            message: "quota response carries no coding window".into(),
            status: None,
        })
    }

    /// Common headers of every request: the key by the configured scheme
    /// and the pinned API version.
    fn authenticated(&self, mut request: HttpRequest) -> HttpRequest {
        if self.bearer_auth {
            request.headers.push((
                "Authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ));
        } else {
            request
                .headers
                .push(("x-api-key".to_owned(), self.api_key.clone()));
        }
        request
            .headers
            .push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
        request
    }
}

/// The api.z.ai host — the one endpoint this adapter decorates, because
/// its `/models` serves ids alone: the Anthropic schema has no room for
/// catalog facts.
const ZAI_HOST: &str = "api.z.ai";

/// The mainland host the same plan is sold through: its
/// Anthropic-compatible route and monitor endpoint answer there too.
const ZAI_CN_HOST: &str = "open.bigmodel.cn";

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

/// The host of a `scheme://host[:port]/...` URL, port stripped — the one
/// piece of URL parsing this crate needs, done by hand so the wasm build
/// carries no URL crate.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    authority.split(':').next()
}

/// The origin of a URL: scheme and authority, no path or query.
fn url_origin(url: &str) -> &str {
    match url.find("//") {
        Some(slashes) => match url[slashes + 2..].find('/') {
            Some(path_start) => &url[..slashes + 2 + path_start],
            None => url,
        },
        None => url,
    }
}

/// The monitor endpoint behind a base URL, when that base is a Z.ai
/// host: the quota API lives beside the Anthropic route on both hosts
/// the plan is sold through, so the base's origin supplies the URL.
fn zai_quota_url(base_url: &str) -> Option<String> {
    let host = url_host(base_url)?;
    if host != ZAI_HOST && host != ZAI_CN_HOST {
        return None;
    }
    Some(format!(
        "{}/api/monitor/usage/quota/limit",
        url_origin(base_url)
    ))
}

/// The 2xx test over the transport's numeric status.
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Reads the whole body. For bounded payloads only: error bodies,
/// listing pages, quota envelopes — never a generation stream.
async fn read_all(response: &mut HttpResponse) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    while let Some(item) = response.body.next().await {
        bytes.extend_from_slice(&item.map_err(transport_error)?);
    }
    Ok(bytes)
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the Anthropic
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`]. A
/// `retry-after` response header rides the message: a rate limit does
/// not retry (the ladder cannot clear a quota window), so the reset time
/// is the one fact the caller needs to decide when to come back.
fn api_error(status: u16, raw: &str, headers: &[(String, String)]) -> ProviderError {
    let retry_after = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        message: match retry_after {
            Some(seconds) => format!(
                "anthropic api error (HTTP {}): {} (retry-after: {})",
                status,
                parsed.message(raw),
                seconds
            ),
            None => format!(
                "anthropic api error (HTTP {}): {}",
                status,
                parsed.message(raw)
            ),
        },
        status: Some(status),
    }
}

/// Maps a transport failure onto [`ProviderError`]: a timeout is its own
/// variant, and every other failure is a network error — its own variant,
/// and the retryable one.
fn transport_error(err: TransportError) -> ProviderError {
    match err {
        TransportError::Timeout => ProviderError::Timeout,
        TransportError::Other(msg) => ProviderError::Network(msg),
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
    let event: StreamEvent = serde_json::from_str(payload).map_err(|err| ProviderError::Api {
        message: format!("malformed SSE chunk: {err}"),
        status: None,
    })?;
    assembler.push(event, sink)
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    /// # Errors
    ///
    /// Fails with [`ProviderError::InvalidRequest`] when the request
    /// cannot map onto the wire, [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx response, and [`ProviderError::Api`] without
    /// one on a malformed stream chunk or a mid-stream error event.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = crate::wire::request_body(&req)?;
        let url = format!("{}/v1/messages", self.base_url);

        let mut response = self
            .transport
            .send(
                self.authenticated(HttpRequest::new("POST", url))
                    .with_json_body(&body),
            )
            .await
            .map_err(transport_error)?;

        if !is_success(response.status) {
            let text = read_all(&mut response).await?;
            return Err(api_error(
                response.status,
                &String::from_utf8_lossy(&text),
                &response.headers,
            ));
        }

        // No overall timeout: a long generation is legitimate, and the
        // transport's read stall guard already bounds silences between
        // chunks where one exists.
        let mut decoder = SseDecoder::new();
        let mut assembler = ChunkAssembler::new();
        while let Some(item) = response.body.next().await {
            let bytes = item.map_err(transport_error)?;
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

    #[test]
    fn the_quota_url_lives_on_the_plan_hosts_and_nowhere_else() {
        assert_eq!(
            zai_quota_url("https://api.z.ai/api/anthropic").as_deref(),
            Some("https://api.z.ai/api/monitor/usage/quota/limit")
        );
        assert_eq!(
            zai_quota_url("https://open.bigmodel.cn/api/anthropic/").as_deref(),
            Some("https://open.bigmodel.cn/api/monitor/usage/quota/limit")
        );
        // Another host's route — a plain Anthropic key or a proxy — has
        // no monitor endpoint to ask, not a wrong one to leak the key to.
        assert_eq!(zai_quota_url("https://api.anthropic.com"), None);
        assert_eq!(zai_quota_url("http://127.0.0.1:8080/api/anthropic"), None);
        // No scheme, no host: refused, not misparsed.
        assert_eq!(zai_quota_url("api.z.ai/api/anthropic"), None);
    }

    #[tokio::test]
    async fn the_quota_call_authenticates_by_bearer_and_parses_the_windows() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // A leaked `anthropic-version` pin lands here first and fails the
        // call: the monitor endpoint is a platform API, not Messages.
        Mock::given(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/monitor/usage/quota/limit"))
            .and(header("Authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 200, "msg": "Operation successful", "success": true,
                "data": { "level": "PRO", "limits": [
                    { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 47 },
                    { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 12 },
                    { "type": "TIME_LIMIT", "unit": 5, "percentage": 38 }
                ] }
            })))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key").with_base_url(server.uri());
        let allowance = provider
            .fetch_zai_quota(&format!("{}/api/monitor/usage/quota/limit", server.uri()))
            .await
            .expect("the quota call succeeds");
        assert_eq!(allowance.amount, "5h 53% wk 88%");
    }

    #[tokio::test]
    async fn a_rejected_token_surfaces_the_envelope_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/monitor/usage/quota/limit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 401, "msg": "token expired or incorrect", "success": false
            })))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key").with_base_url(server.uri());
        let err = provider
            .fetch_zai_quota(&format!("{}/api/monitor/usage/quota/limit", server.uri()))
            .await
            .expect_err("the failure surfaces");
        // The envelope's own code and message carry the failure: the
        // rejected token never rides an HTTP status here.
        match err {
            ProviderError::Api { message, status } => {
                assert_eq!(status, None);
                assert!(message.contains("code 401"), "{message}");
                assert!(message.contains("token expired or incorrect"), "{message}");
            }
            other => panic!("expected an Api error, got {other:?}"),
        }
    }
}
