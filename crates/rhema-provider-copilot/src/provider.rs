//! The [`CopilotProvider`]: the Copilot proxy API's HTTP and SSE
//! transport, with the short-lived token its calls require.
//!
//! Copilot's chat endpoint does not take a personal token. The caller
//! hands the constructor a long-lived GitHub OAuth token (the device
//! flow's output — see [`crate::login`]); the provider exchanges it at
//! `{github}/copilot_internal/v2/token` for a Copilot token that lives
//! minutes, and re-exchanges ahead of expiry — a small margin before
//! `expires_at`, and once more immediately when the chat endpoint
//! answers 401, because a cached token can expire mid-turn. The chat
//! base for every call the token serves comes from the token itself
//! (its `proxy-ep` claim names the per-plan proxy host), falling back
//! to the exchange response's `endpoints.api`, then [`DEFAULT_API_BASE`].
//!
//! The chat and models calls speak the OpenAI dialect — the wire
//! constraints are [`crate::wire`]'s. The proxy rejects requests that
//! do not look like an editor integration, so every call carries the
//! editor headers (`Copilot-Integration-Id`, `Editor-Version`, the
//! `X-GitHub-Api-Version` the endpoints understand) beside the bearer
//! token.
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
use rhema_provider::{Allowance, ModelCard, SseDecoder};
use tokio::sync::Mutex as AsyncMutex;

use crate::wire::{
    ChatCompletionChunk, ChatCompletionRequest, ChunkAssembler, WireCopilotUser, WireErrorBody,
};

/// The GitHub host the OAuth token exchanges at.
pub const DEFAULT_GITHUB_URL: &str = "https://github.com";

/// The chat base assumed before the first exchange answers with the
/// token's own serving host.
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";

/// The SSE payload that ends every OpenAI-compatible chat stream.
const DONE_SENTINEL: &str = "[DONE]";

/// How long before a Copilot token's `expires_at` the cache treats it as
/// spent: one margin refresh costs one exchange; a mid-turn 401 costs a
/// failed request plus a retry.
const EXPIRY_MARGIN: u64 = 300;

/// The editor identity the proxy expects on every call, kept current
/// with the official Copilot Chat extension — the proxy classifies
/// clients by these headers, and a stale-looking version invites the
/// rejection third-party clients have no recourse against.
const EDITOR_VERSION: &str = "vscode/1.107.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
/// The GitHub REST API version the proxy endpoints understand; without
/// it the proxy pins its own (older) default.
const COPILOT_API_VERSION: &str = "2026-06-01";

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

impl std::fmt::Debug for CopilotProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The OAuth token and the cached Copilot token both authenticate:
        // neither prints; the plain data does.
        f.debug_struct("CopilotProvider")
            .field("github_url", &self.github_url)
            .field("api_base", &self.api_base)
            .field("github_token", &"<redacted>")
            .field("http", &self.http)
            .field("cached", &"<redacted>")
            .finish()
    }
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

    /// Answers chat calls at `api_base` instead of the served base. A
    /// test seam: production resolution takes the token's `proxy-ep`
    /// claim, then the exchange response's `endpoints.api`, then this
    /// default.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_owned();
        self
    }

    /// A live Copilot token: the cached one when it outlives the expiry
    /// margin, else a fresh exchange that replaces it.
    async fn copilot_token(&self) -> Result<CopilotToken, ProviderError> {
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
        let page: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed models page: {err}"),
            status: None,
        })?;
        Ok(crate::wire::normalize_models(&page))
    }

    /// The seat's premium-request allowance (`GET
    /// {github}/copilot_internal/user`, authenticated with the OAuth
    /// token — no Copilot-token exchange needed). The endpoint is
    /// GitHub's internal one, outside any contract; a drifted shape
    /// maps to an error and the badge simply never shows.
    pub async fn allowance(&self) -> Result<Allowance, ProviderError> {
        let response = self
            .http
            .get(format!("{}/copilot_internal/user", self.github_url))
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
            .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
    }
}

/// The API base a Copilot token serves, from the `proxy-ep` host claim
/// its semicolon-separated body carries:
/// `proxy.<plan>.githubcopilot.com` serves at
/// `https://api.<plan>.githubcopilot.com`. Only GitHub's own hosts are
/// honored — the claim is meaningless for any other host, and honoring
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

/// Unix seconds now.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
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
            let mut cached = provider.cached.lock().await;
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
        assert!(printed.contains("github_url"), "{printed}");
        assert!(printed.contains("api_base"), "{printed}");
    }

    #[test]
    fn a_claim_off_githubs_hosts_is_ignored() {
        // Honoring a foreign claim would point the chat calls wherever
        // the token says — the claim is only meaningful for GitHub's
        // own hosts.
        assert_eq!(
            api_base_from_token("tid=abc;proxy-ep=proxy.evil.example.com"),
            None
        );
        assert_eq!(api_base_from_token("tid=abc;exp=1;ctu=1;"), None);
    }
}
