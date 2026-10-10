//! The [`CopilotProvider`]: the Copilot proxy API's HTTP and SSE
//! transport, with the short-lived token its calls require.
//!
//! Copilot's chat endpoint does not take a personal token. The caller
//! hands the constructor a long-lived GitHub OAuth token (the device
//! flow's output — see [`crate::login`]); the provider exchanges it at
//! `{github}/copilot_internal/v2/token` for a Copilot token that lives
//! minutes, and re-exchanges ahead of expiry — a small margin before
//! `expires_at`, and once more immediately when the chat endpoint
//! answers 401, because a cached token can expire mid-turn. The exchange
//! response also carries the chat base (`endpoints.api`), which wins
//! over [`DEFAULT_API_BASE`] for every call the token serves.
//!
//! The chat and models calls speak the OpenAI dialect — the wire
//! constraints are [`crate::wire`]'s. The proxy rejects requests that
//! do not look like an editor integration, so every call carries the
//! editor headers (`Copilot-Integration-Id`, `Editor-Version`) beside
//! the bearer token.
//!
//! Timeouts follow the sibling adapters: connect capped (10s), stream
//! reads stall-guarded (90s — a stall guard, not a duration cap), and
//! the non-streaming calls (token exchange, models list) under a 20s
//! overall timeout. Streaming calls get no total timeout.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{ModelCard, SseDecoder};
use tokio::sync::Mutex as AsyncMutex;

use crate::wire::{ChatCompletionChunk, ChatCompletionRequest, ChunkAssembler, WireErrorBody};

/// The GitHub host the OAuth token exchanges at.
pub const DEFAULT_GITHUB_URL: &str = "https://github.com";

/// The chat base assumed before the first exchange answers with the
/// token's own `endpoints.api`.
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// How long before a Copilot token's `expires_at` the cache treats it as
/// spent: one margin refresh costs one exchange; a mid-turn 401 costs a
/// failed request plus a retry.
const EXPIRY_MARGIN: u64 = 300;

/// The editor identity the proxy expects on every call.
const EDITOR_VERSION: &str = "vscode/1.99.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.4.0";

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

/// The GitHub Copilot chat models, driven by a GitHub OAuth token the
/// provider exchanges for short-lived Copilot tokens as needed.
pub struct CopilotProvider {
    github_url: String,
    api_base: String,
    github_token: String,
    http: reqwest::Client,
    /// The cached Copilot token, shared across calls: exchanges are
    /// serialized on it — the lock is held across the exchange — so a
    /// burst of steps cannot stampede the endpoint.
    cached: AsyncMutex<Option<CopilotToken>>,
}

impl CopilotProvider {
    /// A provider exchanging `github_token` — the device flow's
    /// long-lived output — against [`DEFAULT_GITHUB_URL`].
    pub fn new(github_token: impl Into<String>) -> Self {
        Self {
            github_url: DEFAULT_GITHUB_URL.to_owned(),
            api_base: DEFAULT_API_BASE.to_owned(),
            github_token: github_token.into(),
            http: http_client(),
            cached: AsyncMutex::new(None),
        }
    }

    /// Targets `github_url` for the token exchange instead of
    /// [`DEFAULT_GITHUB_URL`] (trailing slashes trimmed). A test seam:
    /// production code never calls this.
    pub fn with_github_url(mut self, github_url: impl Into<String>) -> Self {
        self.github_url = github_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Answers chat calls at `api_base` instead of [`DEFAULT_API_BASE`].
    /// A test seam over the exchange's `endpoints.api`, which still wins
    /// when the first exchange answers.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_owned();
        self
    }

    /// A live Copilot token: the cached one when it outlives the expiry
    /// margin, else a fresh exchange that replaces it.
    async fn copilot_token(&self) -> Result<CopilotToken, ProviderError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref() {
            if now_epoch() + EXPIRY_MARGIN < token.expires_at {
                return Ok(token.clone());
            }
        }
        let fresh = self.exchange().await?;
        *cached = Some(fresh.clone());
        Ok(fresh)
    }

    /// Exchanges the GitHub OAuth token for a Copilot token. Callers
    /// hold the cached-token lock across this, which serializes
    /// exchanges.
    async fn exchange(&self) -> Result<CopilotToken, ProviderError> {
        let response = self
            .http
            .get(format!("{}/copilot_internal/v2/token", self.github_url))
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
        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|err| ProviderError::Api(format!("malformed token response: {err}")))?;
        if parsed.token.is_empty() {
            return Err(ProviderError::Api("token response carries no token".into()));
        }
        let api_base = parsed
            .endpoints
            .map(|endpoints| endpoints.api)
            .filter(|api| !api.is_empty())
            .unwrap_or_else(|| self.api_base.to_owned());
        Ok(CopilotToken {
            token: parsed.token,
            expires_at: parsed.expires_at,
            api_base: api_base.trim_end_matches('/').to_owned(),
        })
    }

    /// The models the seat's Copilot subscription serves (`GET
    /// {api}/models`), text chat models only — see
    /// [`crate::wire::normalize_models`].
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let token = self.copilot_token().await?;
        let base = token.api_base.clone();
        let response = self
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
        let page: serde_json::Value = response
            .json()
            .await
            .map_err(|err| ProviderError::Api(format!("malformed models page: {err}")))?;
        Ok(crate::wire::normalize_models(&page))
    }
}

/// The editor headers every proxy call carries.
trait EditorHeaders {
    /// Attaches the editor identity headers.
    fn editor_headers(self) -> reqwest::RequestBuilder;
}

impl EditorHeaders for reqwest::RequestBuilder {
    fn editor_headers(self) -> reqwest::RequestBuilder {
        self.header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
            .header(reqwest::header::USER_AGENT, EDITOR_PLUGIN_VERSION)
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it.
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api(format!(
        "copilot api error (HTTP {}): {}",
        status.as_u16(),
        parsed.message(raw)
    ))
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot
/// be carried in [`ProviderError`] directly (it is `Clone + Eq`).
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

/// Unix seconds now.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl ModelProvider for CopilotProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let body = crate::wire::request_body(&req);
        let mut token = self.copilot_token().await?;
        let mut response = self.post_chat(&token, &body).await?;

        // A 401 mid-turn means the cached token died between the margin
        // check and the call: one forced refresh and retry — a second
        // failure is the server's answer, not a refresh problem.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            *self.cached.lock().await = None;
            token = self.copilot_token().await?;
            response = self.post_chat(&token, &body).await?;
        }

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

impl CopilotProvider {
    /// Sends one chat-completions request with `token`'s credentials.
    async fn post_chat(
        &self,
        token: &CopilotToken,
        body: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/chat/completions", token.api_base);
        self.http
            .post(&url)
            .bearer_auth(&token.token)
            .editor_headers()
            .json(body)
            .send()
            .await
            .map_err(network_error)
    }
}
