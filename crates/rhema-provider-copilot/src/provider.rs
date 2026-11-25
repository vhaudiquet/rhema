//! The [`CopilotProvider`]: the Copilot proxy API's HTTP and SSE
//! transport, with the short-lived token its calls require.
//!
//! Copilot's chat endpoint does not take a personal token. The caller
//! hands the constructor a long-lived GitHub OAuth token (the device
//! flow's output, see [`crate::login`]); the provider exchanges it at
//! `{api}/copilot_internal/v2/token`, the internal endpoints answer on
//! GitHub's REST API host ([`DEFAULT_GITHUB_API_URL`], not github.com,
//! which serves only the device flow), for a Copilot token that lives
//! minutes, and re-exchanges ahead of expiry, a small margin before
//! `expires_at`, and once more immediately when the chat endpoint
//! answers 401, because a cached token can expire mid-turn. The chat
//! base for every call the token serves comes from the token itself
//! (its `proxy-ep` claim names the per-plan proxy host), falling back
//! to the exchange response's `endpoints.api`, then [`DEFAULT_API_BASE`].
//!
//! The chat, Responses, and models calls speak the OpenAI dialect, the
//! wire constraints are [`crate::wire`]'s; the route is learned from the
//! models page's `supported_endpoints` and from the chat endpoint's own
//! rejection. The proxy rejects requests that
//! do not look like an editor integration, so every call carries the
//! editor headers (`Copilot-Integration-Id`, `Editor-Version`, the
//! `X-GitHub-Api-Version` the endpoints understand) beside the bearer
//! token.
//!
//! Timeouts follow the sibling adapters: connect capped (10s), stream
//! reads stall-guarded (90s, a stall guard, not a duration cap), and
//! the non-streaming calls (token exchange, models list) under a 20s
//! overall timeout. Streaming calls get no total timeout.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{
    Allowance, HttpRequest, HttpResponse, ModelCard, ModelListing, Transport, TransportError,
};
use rhema_provider_anthropic::{AnthropicProvider, ReqwestTransport};
use rhema_provider_openai_compatible::provider::{
    drain_chat_stream, drain_responses_stream, network_error as dialect_network_error,
};
use rhema_provider_openai_compatible::responses::{
    ResponsesRequest, request_body as responses_request_body,
};
use rhema_provider_openai_compatible::wire::{
    ChatCompletionRequest, OPENAI_COMPATIBLE, WireErrorBody, request_body,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::wire::WireCopilotUser;

/// The GitHub host the OAuth device flow ([`crate::login`]) runs
/// against.
pub const DEFAULT_GITHUB_URL: &str = "https://github.com";

/// The GitHub REST API host the internal Copilot endpoints answer on:
/// the token exchange and the user lookup live on api.github.com, and
/// github.com serves none of them.
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// The chat base assumed before the first exchange answers with the
/// token's own serving host.
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";

/// How long before a Copilot token's `expires_at` the cache treats it as
/// spent: one margin refresh costs one exchange; a mid-turn 401 costs a
/// failed request plus a retry.
const EXPIRY_MARGIN: u64 = 300;

/// The editor identity the proxy expects on every call, kept current
/// with the official Copilot Chat extension, the proxy classifies
/// clients by these headers, and a stale-looking version invites the
/// rejection third-party clients have no recourse against.
const EDITOR_VERSION: &str = "vscode/1.107.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
/// The GitHub REST API version the proxy endpoints understand; without
/// it the proxy pins its own (older) default.
const COPILOT_API_VERSION: &str = "2026-06-01";

/// The HTTP client every instance shares: bounded connects and name
/// lookups, stream reads stall-guarded, never total-time capped.
fn http_client() -> reqwest::Client {
    rhema_provider::http::client_builder()
        .build()
        .expect("the shared client builder is valid")
}

/// A live Copilot token and the chat base it is valid on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CopilotToken {
    token: String,
    /// Unix seconds the token dies at.
    expires_at: u64,
    api_base: String,
}

/// The exchange endpoint's response body. The `refresh_in` advisory is
/// ignored: the deadline is `expires_at`, and the margin plus the 401
/// retry cover the gap a slow server leaves.
#[derive(Debug, Default, serde::Deserialize)]
struct TokenResponse {
    /// The short-lived Copilot token.
    #[serde(default)]
    token: String,
    /// Unix seconds the token dies at.
    #[serde(default)]
    expires_at: u64,
    /// The endpoints the token serves; `api` is the chat base.
    #[serde(default)]
    endpoints: Option<WireEndpoints>,
}

/// The `endpoints` object of a [`TokenResponse`].
#[derive(Debug, Default, serde::Deserialize)]
struct WireEndpoints {
    /// The chat API base.
    #[serde(default)]
    api: String,
}

/// The GitHub OAuth token exchange and the cached short-lived Copilot
/// token, shared between every route that needs a token at send time:
/// the OpenAI-shaped chat route, the models page, and the
/// Anthropic-shaped messages route's transport.
#[derive(Clone)]
struct TokenSource {
    github_api_url: String,
    api_base: String,
    github_token: String,
    http: reqwest::Client,
    /// The cached Copilot token, shared across calls: exchanges are
    /// serialized on it, the lock is held across the exchange, so a
    /// burst of steps cannot stampede the endpoint.
    cached: Arc<AsyncMutex<Option<CopilotToken>>>,
}

impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The OAuth token and the cached Copilot token both
        // authenticate: neither prints; the plain data does.
        f.debug_struct("TokenSource")
            .field("github_api_url", &self.github_api_url)
            .field("api_base", &self.api_base)
            .field("github_token", &"<redacted>")
            .field("http", &self.http)
            .field("cached", &"<redacted>")
            .finish()
    }
}

impl TokenSource {
    fn new(github_api_url: &str, api_base: &str, github_token: &str) -> Self {
        Self {
            github_api_url: github_api_url.to_owned(),
            api_base: api_base.to_owned(),
            github_token: github_token.to_owned(),
            http: http_client(),
            cached: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// A live Copilot token: the cached one when it outlives the expiry
    /// margin, else a fresh exchange that replaces it.
    async fn fresh(&self) -> Result<CopilotToken, ProviderError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref()
            && now_epoch() + EXPIRY_MARGIN < token.expires_at
        {
            return Ok(token.clone());
        }
        let fresh = self.exchange().await?;
        *cached = Some(fresh.clone());
        Ok(fresh)
    }

    /// Drops the cached token: the next `fresh` exchanges again. The
    /// 401 path calls this, a token that died between the margin check
    /// and the call it authenticated.
    async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    /// Exchanges the GitHub OAuth token for a Copilot token. Callers
    /// hold the cached-token lock across this, which serializes
    /// exchanges.
    async fn exchange(&self) -> Result<CopilotToken, ProviderError> {
        let response = self
            .http
            .get(format!("{}/copilot_internal/v2/token", self.github_api_url))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", self.github_token),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .editor_headers()
            // A bounded listing-style call, not a stream.
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        let parsed: TokenResponse =
            serde_json::from_str(&text).map_err(|err| ProviderError::Api {
                message: format!("malformed token response: {err}"),
                status: None,
            })?;
        if parsed.token.is_empty() {
            return Err(ProviderError::Api {
                message: "token response carries no token".into(),
                status: None,
            });
        }
        let api_base = api_base_from_token(&parsed.token)
            .or_else(|| {
                parsed
                    .endpoints
                    .map(|endpoints| endpoints.api)
                    .filter(|api| !api.is_empty())
            })
            .unwrap_or_else(|| self.api_base.to_owned());
        Ok(CopilotToken {
            token: parsed.token,
            expires_at: parsed.expires_at,
            api_base: api_base.trim_end_matches('/').to_owned(),
        })
    }
}

impl CopilotProvider {
    /// The seat's premium-request allowance (`GET
    /// {api}/copilot_internal/user`, authenticated with the OAuth
    /// token, no Copilot-token exchange needed). The endpoint is
    /// GitHub's internal one, outside any contract; a drifted shape
    /// maps to an error and the badge simply never shows.
    pub async fn allowance(&self) -> Result<Allowance, ProviderError> {
        self.tokens.allowance().await
    }
}

impl TokenSource {
    /// The seat's premium-request allowance, the shared source's own
    /// view of the seat.
    async fn allowance(&self) -> Result<Allowance, ProviderError> {
        let response = self
            .http
            .get(format!("{}/copilot_internal/user", self.github_api_url))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", self.github_token),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .editor_headers()
            // A bounded listing-style call, not a stream.
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        let user: WireCopilotUser =
            serde_json::from_str(&text).map_err(|err| ProviderError::Api {
                message: format!("malformed user response: {err}"),
                status: None,
            })?;
        crate::wire::allowance(&user).ok_or_else(|| ProviderError::Api {
            message: "user response carries no premium quota".into(),
            status: None,
        })
    }
}

/// The editor identity as plain pairs, the same five headers the
/// reqwest extension attaches, so the Messages transport can carry
/// them through a seam that builds its own request.
fn editor_header_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Editor-Version", EDITOR_VERSION),
        ("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION),
        ("Copilot-Integration-Id", COPILOT_INTEGRATION_ID),
        ("X-GitHub-Api-Version", COPILOT_API_VERSION),
        (reqwest::header::USER_AGENT.as_str(), USER_AGENT),
    ]
}

/// The editor headers every proxy call carries.
trait EditorHeaders {
    /// Attaches the editor identity headers.
    fn editor_headers(self) -> reqwest::RequestBuilder;
}

impl EditorHeaders for reqwest::RequestBuilder {
    fn editor_headers(self) -> reqwest::RequestBuilder {
        let mut builder = self;
        for (name, value) in editor_header_pairs() {
            builder = builder.header(name, value);
        }
        builder
    }
}

/// The GitHub Copilot chat models, driven by a GitHub OAuth token the
/// provider exchanges for short-lived Copilot tokens as needed.
pub struct CopilotProvider {
    tokens: Arc<TokenSource>,
    /// The Anthropic-shaped Messages route for Claude models, built on
    /// first use: its transport draws the current Copilot token from
    /// `tokens` at send time, and the wire is the native Anthropic
    /// adapter's, breakpoints included.
    messages: OnceLock<Arc<AnthropicProvider>>,
    /// Model ids the proxy serves only on the Responses API, learned
    /// from the models page's `supported_endpoints` and from the chat
    /// endpoint's own rejection. One provider serves every listed model,
    /// so the route is remembered per id.
    responses_only: Arc<AsyncMutex<HashSet<String>>>,
    /// Model ids that refuse the reasoning field, learned from the
    /// rejection so later turns drop it before sending instead of
    /// paying a 400 each time.
    no_reasoning: Arc<AsyncMutex<HashSet<String>>>,
}

impl std::fmt::Debug for CopilotProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The OAuth token and the cached Copilot token both authenticate:
        // neither prints; the plain data does.
        f.debug_struct("CopilotProvider")
            .field("tokens", &self.tokens)
            .field("messages", &self.messages)
            .field("responses_only", &self.responses_only)
            .field("no_reasoning", &self.no_reasoning)
            .finish()
    }
}

impl CopilotProvider {
    /// A provider exchanging `github_token`, the device flow's
    /// long-lived output, against [`DEFAULT_GITHUB_API_URL`].
    pub fn new(github_token: impl Into<String>) -> Self {
        Self {
            tokens: Arc::new(TokenSource::new(
                DEFAULT_GITHUB_API_URL,
                DEFAULT_API_BASE,
                &github_token.into(),
            )),
            messages: OnceLock::new(),
            responses_only: Arc::new(AsyncMutex::new(HashSet::new())),
            no_reasoning: Arc::new(AsyncMutex::new(HashSet::new())),
        }
    }

    /// Targets `github_api_url` for the internal endpoints instead of
    /// [`DEFAULT_GITHUB_API_URL`] (trailing slashes trimmed). A test
    /// seam: production code never calls this.
    pub fn with_github_api_url(mut self, github_api_url: impl Into<String>) -> Self {
        let mut tokens = (*self.tokens).clone();
        tokens.github_api_url = github_api_url.into().trim_end_matches('/').to_owned();
        self.tokens = Arc::new(tokens);
        self
    }

    /// Answers chat calls at `api_base` instead of the served base. A
    /// test seam: production resolution takes the token's `proxy-ep`
    /// claim, then the exchange response's `endpoints.api`, then this
    /// default.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        let mut tokens = (*self.tokens).clone();
        tokens.api_base = api_base.into().trim_end_matches('/').to_owned();
        self.tokens = Arc::new(tokens);
        self
    }

    /// The Messages route: the native Anthropic adapter over a
    /// transport that rewrites every request onto the token's served
    /// base with the current Copilot token. The base the provider
    /// itself holds is a placeholder, replaced at send time; the
    /// adapter's own bearer key is replaced the same way.
    fn messages_provider(&self) -> Arc<AnthropicProvider> {
        self.messages
            .get_or_init(|| {
                let transport = MessagesTransport {
                    tokens: self.tokens.clone(),
                    inner: ReqwestTransport::new(),
                };
                Arc::new(
                    AnthropicProvider::new_with_transport("", Arc::new(transport))
                        .with_bearer_auth(),
                )
            })
            .clone()
    }

    /// Completes a Claude-model turn over the Messages route. A 401
    /// means the token died between the margin check and the call it
    /// authenticated: force one refresh and retry, the same chore the
    /// chat route runs; a second failure is the server's answer.
    async fn complete_messages(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let inner = self.messages_provider();
        match inner.complete(req.clone(), sink).await {
            Err(ProviderError::Api {
                status: Some(401), ..
            }) => {
                self.tokens.invalidate().await;
                inner.complete(req, sink).await
            }
            result => result,
        }
    }
}

/// The transport under the Messages route: the native Anthropic
/// adapter hands over a fully formed request carrying its own bearer
/// key and placeholder base, and this rewrites it onto the Copilot
/// endpoint the current token serves, with the editor identity the
/// other routes carry. The retry lives above the seam, in
/// `complete_messages`.
struct MessagesTransport {
    tokens: Arc<TokenSource>,
    inner: ReqwestTransport,
}

impl MessagesTransport {
    /// Aims one request at Copilot's Messages endpoint: the token's
    /// served base, the current token as the bearer, the editor
    /// identity. Idempotent, the 401 retry aims the same request twice.
    fn aim(&self, request: &mut HttpRequest, token: &CopilotToken) {
        // The endpoint Copilot serves Claude models at, Anthropic
        // Messages shape; GitHub's own client routes them there too.
        request.url = format!("{}/v1/messages", token.api_base);
        for (name, value) in request.headers.iter_mut() {
            if name.eq_ignore_ascii_case("authorization") {
                *value = format!("Bearer {}", token.token);
            }
        }
        // Set, not pushed twice: the retry aims an already-aimed
        // request.
        for (name, value) in editor_header_pairs() {
            let carried = request
                .headers
                .iter_mut()
                .find(|(carried, _)| carried.eq_ignore_ascii_case(name));
            match carried {
                Some((_, carried)) => *carried = value.to_owned(),
                None => request.headers.push((name.to_owned(), value.to_owned())),
            }
        }
    }
}

#[async_trait::async_trait]
impl Transport for MessagesTransport {
    async fn send(&self, mut request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let token = self
            .tokens
            .fresh()
            .await
            .map_err(|err| TransportError::Other(err.to_string()))?;
        self.aim(&mut request, &token);
        self.inner.send(request).await
    }
}

/// The API base a Copilot token serves, from the `proxy-ep` host claim
/// its semicolon-separated body carries:
/// `proxy.<plan>.githubcopilot.com` serves at
/// `https://api.<plan>.githubcopilot.com`. Only GitHub's own hosts are
/// honored, the claim is meaningless for any other host, and honoring
/// it would let a malformed token point the chat calls somewhere else.
/// `None` when the token carries no usable claim, falling back to the
/// exchange response's `endpoints.api`.
fn api_base_from_token(token: &str) -> Option<String> {
    let host = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))?
        .trim();
    if !host.ends_with(".githubcopilot.com") {
        return None;
    }
    let host = match host.strip_prefix("proxy.") {
        Some(rest) => format!("api.{rest}"),
        None => host.to_owned(),
    };
    Some(format!("https://{host}"))
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        message: format!(
            "copilot api error (HTTP {}): {}",
            status.as_u16(),
            parsed.message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot
/// be carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    dialect_network_error(err)
}

/// Unix seconds now.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl ModelListing for CopilotProvider {
    /// The models the seat's Copilot subscription serves (`GET
    /// {api}/models`), chat-capable rows only, see
    /// [`crate::wire::normalize_models`].
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let token = self.tokens.fresh().await?;
        let base = token.api_base.clone();
        let response = self
            .tokens
            .http
            .get(format!("{base}/models"))
            .bearer_auth(&token.token)
            .editor_headers()
            // A bounded listing call, not a stream.
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
        // Learn the endpoint each model answers on before dropping the
        // page: the rows the pager shows carry it, and a later turn has
        // no other place to read it.
        for entry in page
            .get("data")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if crate::wire::model_route(entry) == Some(crate::wire::ChatRoute::Responses) {
                self.remember_responses(id).await;
            }
        }
        Ok(crate::wire::normalize_models(&page))
    }
}

#[async_trait]
impl ModelProvider for CopilotProvider {
    /// # Errors
    ///
    /// Fails with [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport or the token exchange fails, [`ProviderError::Api`]
    /// with the HTTP status on a non-2xx response (one 401-driven token
    /// refresh and retry included), and [`ProviderError::Api`] without
    /// one on a malformed stream chunk or a mid-stream error chunk.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let mut req = req.normalized();
        // A model that refused the reasoning field keeps the turn cheap:
        // the level is dropped before the request, not after a 400.
        if req.params.thinking != rhema_core::request::ThinkingEffort::Default
            && self.rejects_reasoning(&req.params.model).await
        {
            req.params.thinking = rhema_core::request::ThinkingEffort::Default;
        }
        // Claude models: Copilot serves them in Anthropic Messages
        // shape at /v1/messages, where prompt caching needs
        // caller-placed breakpoints; the chat-completions route sends
        // neither that format nor that caching. The native Anthropic
        // wire runs behind a token-swapping transport, breakpoints and
        // thinking mapping included.
        if req.params.model.starts_with("claude") {
            return self.complete_messages(req, sink).await;
        }
        // The proxy serves a growing set of models on the Responses API
        // alone. The models page names them ahead of time; a turn that
        // still reaches the chat endpoint learns it from the rejection
        // and stays on the route after that.
        if self.needs_responses(&req.params.model).await {
            return self.complete_responses(&req, sink).await;
        }
        let body = request_body(&req, &OPENAI_COMPATIBLE);
        let mut token = self.tokens.fresh().await?;
        let mut response = self.post_chat(&token, &body).await?;

        // A 401 mid-turn means the cached token died between the margin
        // check and the call: one forced refresh and retry, a second
        // failure is the server's answer, not a refresh problem.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.tokens.invalidate().await;
            token = self.tokens.fresh().await?;
            response = self.post_chat(&token, &body).await?;
        }

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST {
                if names_another_endpoint(&text) {
                    self.remember_responses(&req.params.model).await;
                    return self.complete_responses(&req, sink).await;
                }
                // The proxy validates the reasoning field against the
                // model's capabilities. A model that takes none (the
                // editor sends it only where the models page lists a
                // level) must not fail the turn over the default level.
                if reasoning_unsupported(&text)
                    && req.params.thinking != rhema_core::request::ThinkingEffort::Default
                {
                    self.remember_no_reasoning(&req.params.model).await;
                    let mut plain = req.clone();
                    plain.params.thinking = rhema_core::request::ThinkingEffort::Default;
                    return self.complete(plain, sink).await;
                }
            }
            return Err(api_error(status, &text));
        }

        // No overall timeout: a long generation is legitimate, and the
        // client's read timeout already bounds stalls between chunks.
        drain_chat_stream(response, &OPENAI_COMPATIBLE, sink).await
    }
}

impl CopilotProvider {
    /// Whether the proxy serves `model` only on the Responses API.
    async fn needs_responses(&self, model: &str) -> bool {
        self.responses_only.lock().await.contains(model)
    }

    /// Records that `model` answers on the Responses API, so every later
    /// turn skips the chat endpoint.
    async fn remember_responses(&self, model: &str) {
        self.responses_only.lock().await.insert(model.to_owned());
    }

    /// Whether `model` refused the reasoning field on an earlier turn.
    async fn rejects_reasoning(&self, model: &str) -> bool {
        self.no_reasoning.lock().await.contains(model)
    }

    /// Records that `model` takes no reasoning field, so every later
    /// turn drops it before sending.
    async fn remember_no_reasoning(&self, model: &str) {
        self.no_reasoning.lock().await.insert(model.to_owned());
    }

    /// Completes a turn on the Responses-API route, the one the proxy
    /// serves newer OpenAI models on. The token refresh chore is the chat
    /// route's: a 401 forces one exchange and one retry.
    async fn complete_responses(
        &self,
        req: &ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = responses_request_body(req, &OPENAI_COMPATIBLE);
        let mut token = self.tokens.fresh().await?;
        let mut response = self.post_responses(&token, &body).await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.tokens.invalidate().await;
            token = self.tokens.fresh().await?;
            response = self.post_responses(&token, &body).await?;
        }

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // Same fallback as the chat route: a model that takes no
            // reasoning still runs with the field dropped.
            if status == reqwest::StatusCode::BAD_REQUEST
                && reasoning_unsupported(&text)
                && req.params.thinking != rhema_core::request::ThinkingEffort::Default
            {
                self.remember_no_reasoning(&req.params.model).await;
                let mut plain = req.clone();
                plain.params.thinking = rhema_core::request::ThinkingEffort::Default;
                return self.complete(plain, sink).await;
            }
            return Err(api_error(status, &text));
        }

        drain_responses_stream(response, sink).await
    }

    /// Sends one chat-completions request with `token`'s credentials.
    async fn post_chat(
        &self,
        token: &CopilotToken,
        body: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/chat/completions", token.api_base);
        self.tokens
            .http
            .post(&url)
            .bearer_auth(&token.token)
            .editor_headers()
            .json(body)
            .send()
            .await
            .map_err(network_error)
    }

    /// Sends one Responses-API request with `token`'s credentials.
    async fn post_responses(
        &self,
        token: &CopilotToken,
        body: &ResponsesRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/responses", token.api_base);
        self.tokens
            .http
            .post(&url)
            .bearer_auth(&token.token)
            .editor_headers()
            .json(body)
            .send()
            .await
            .map_err(network_error)
    }
}

/// Whether the chat endpoint's rejection names another endpoint for the
/// model, rather than a real fault in the request: the proxy's wording
/// when a model answers only on the Responses API.
fn names_another_endpoint(raw: &str) -> bool {
    raw.to_ascii_lowercase()
        .contains("not accessible via the /chat/completions")
}

/// Whether the proxy's rejection names the reasoning field as one the
/// model does not take, rather than a real fault in the request.
fn reasoning_unsupported(raw: &str) -> bool {
    let message = raw.to_ascii_lowercase();
    // Every spelling of the field carries `reasoning`; the refusals
    // below name it alone when the proxy drops the key.
    let names = message.contains("reasoning");
    names
        && (message.contains("not supported")
            || message.contains("unsupported")
            || message.contains("unrecognized")
            || message.contains("unknown"))
}

#[cfg(test)]
mod tests {
    use super::{names_another_endpoint, reasoning_unsupported};

    #[test]
    fn the_reasoning_detector_only_reads_refusals_of_that_field() {
        assert!(reasoning_unsupported(
            r#"{"error":{"message":"Unsupported parameter: 'reasoning_effort' is not supported with this model."}}"#
        ));
        assert!(reasoning_unsupported(
            r#"{"error":{"message":"Unknown parameter: reasoning"}}"#
        ));
        assert!(!reasoning_unsupported(
            r#"{"error":{"message":"model \"gpt-6-luna\" is not accessible via the /chat/completions endpoint"}}"#
        ));
        assert!(!reasoning_unsupported(
            r#"{"error":{"message":"bad request"}}"#
        ));
        assert!(!names_another_endpoint("bad request"));
    }

    use super::*;

    #[test]
    fn the_proxy_ep_claim_names_the_serving_host() {
        let token = "tid=abc;exp=1;ctu=1;proxy-ep=proxy.individual.githubcopilot.com;";
        assert_eq!(
            api_base_from_token(token).as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        // A business plan's claim maps the same way.
        let token = "tid=abc;proxy-ep=proxy.business.githubcopilot.com";
        assert_eq!(
            api_base_from_token(token).as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[tokio::test]
    async fn debug_prints_neither_the_oauth_token_nor_the_cached_one() {
        let provider = CopilotProvider::new("gho_long_lived_secret");
        {
            let mut cached = provider.tokens.cached.lock().await;
            *cached = Some(CopilotToken {
                token: "copilot_short_lived_secret".into(),
                expires_at: u64::MAX,
                api_base: "https://api.githubcopilot.com".into(),
            });
        }
        let printed = format!("{provider:?}");
        assert!(!printed.contains("gho_long_lived_secret"), "{printed}");
        assert!(!printed.contains("copilot_short_lived_secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // The plain data stays visible.
        assert!(printed.contains("github_api_url"), "{printed}");
        assert!(printed.contains("api_base"), "{printed}");
    }

    #[test]
    fn a_claim_off_githubs_hosts_is_ignored() {
        // Honoring a foreign claim would point the chat calls wherever
        // the token says, the claim is only meaningful for GitHub's
        // own hosts.
        assert_eq!(
            api_base_from_token("tid=abc;proxy-ep=proxy.evil.example.com"),
            None
        );
        assert_eq!(api_base_from_token("tid=abc;exp=1;ctu=1;"), None);
    }
}
