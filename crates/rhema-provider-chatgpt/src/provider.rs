//! The [`ChatGptProvider`]: the ChatGPT backend's HTTP and SSE
//! transport, and the access token's refresh.
//!
//! The caller hands the constructor one credential string, the login
//! flow's JSON, or a bare access token (see
//! [`ChatGptCredential::parse`]). The provider calls
//! `POST {api}/responses` with the bearer token, the
//! `chatgpt-account-id` header, and the `originator: rhema` identity
//! the backend uses for client attribution. A token inside the refresh
//! margin is refreshed before the call; a 401 mid-turn refreshes once
//! and retries, because a cached token can expire between the margin
//! check and the request. Refreshed tokens live in memory for the
//! process lifetime. A frontend that persists them can re-login when
//! the stored refresh token stops working.
//!
//! Timeouts follow the sibling adapters: connect capped (10s), stream
//! reads stall-guarded at 90s, which bounds stalls and not duration, and
//! the non-streaming calls (refresh, models list) under a 20s overall
//! timeout. Streaming calls get no total timeout.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::{ModelCard, ModelListing, SseDecoder};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::credential::{ChatGptCredential, TokenResponse, now_epoch};
use crate::login::{DEFAULT_CLIENT_ID, DEFAULT_TOKEN_URL};
use crate::wire::{StreamAssembler, normalize_models, request_body};

/// The ChatGPT backend base every Responses call and model listing
/// uses.
pub const DEFAULT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// The Codex CLI client version the backend's model catalog is keyed
/// on. The endpoint filters its rows by the client version it is
/// asked for; this adapter speaks the catalog of a recent Codex CLI,
/// not an identity claim.
pub const MODELS_CLIENT_VERSION: &str = "0.167.0";

/// The `originator` header this client identifies itself with.
const ORIGINATOR: &str = "rhema";

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

/// The ChatGPT subscription backend, driven by an OAuth access token
/// the provider refreshes as needed.
pub struct ChatGptProvider {
    credential: AsyncMutex<ChatGptCredential>,
    http: reqwest::Client,
    api_base: String,
    token_url: String,
    /// Stable for the provider's lifetime: one conversation's turns
    /// share a prompt cache.
    prompt_cache_key: String,
}

impl std::fmt::Debug for ChatGptProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential authenticates; it never prints.
        f.debug_struct("ChatGptProvider")
            .field("credential", &"<redacted>")
            .field("http", &self.http)
            .field("api_base", &self.api_base)
            .field("token_url", &self.token_url)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}

impl ChatGptProvider {
    /// A provider on `credential`, which is the login flow's `Completed` JSON,
    /// or a bare access token.
    pub fn new(credential: impl Into<String>) -> Self {
        Self {
            credential: AsyncMutex::new(ChatGptCredential::parse(&credential.into())),
            http: http_client(),
            api_base: DEFAULT_API_BASE.to_owned(),
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            prompt_cache_key: crate::login::random_token(),
        }
    }

    /// Answers calls at `api_base` instead of [`DEFAULT_API_BASE`]
    /// (trailing slashes trimmed). A test seam: production code never
    /// calls this.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_owned();
        self
    }

    /// Refreshes at `token_url` instead of the issuer's
    /// [`DEFAULT_TOKEN_URL`]. A test seam: production code never calls
    /// this.
    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// The bearer token and account id to use, refreshing proactively
    /// when the token is inside the expiry margin.
    async fn token(&self, force: bool) -> Result<(String, Option<String>), ProviderError> {
        let mut credential = self.credential.lock().await;
        let expired = credential.needs_refresh(now_epoch());
        if (force || expired) && credential.refresh_token.is_some() {
            let refreshed = self.refresh(&credential).await?;
            *credential = refreshed;
        }
        Ok((
            credential.access_token.clone(),
            credential.account_id.clone(),
        ))
    }

    /// Whether a refresh token is available for a 401 retry.
    async fn can_refresh(&self) -> bool {
        self.credential.lock().await.refresh_token.is_some()
    }

    /// Exchanges the refresh token for a fresh access token. Callers
    /// hold the credential lock across this, which serializes
    /// refreshes.
    async fn refresh(
        &self,
        credential: &ChatGptCredential,
    ) -> Result<ChatGptCredential, ProviderError> {
        let Some(refresh_token) = credential.refresh_token.as_deref() else {
            return Err(ProviderError::Api {
                message: "chatgpt credential carries no refresh token; sign in again".to_owned(),
                status: None,
            });
        };
        let response = self
            .http
            .post(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", DEFAULT_CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            // A bounded exchange, not a stream.
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
                message: format!("malformed chatgpt refresh response: {err}"),
                status: None,
            })?;
        if parsed.access_token.is_empty() {
            return Err(ProviderError::Api {
                message: "the chatgpt refresh response carries no access token".to_owned(),
                status: None,
            });
        }
        let refresh_token = parsed
            .refresh_token
            .filter(|token| !token.is_empty())
            .or_else(|| credential.refresh_token.clone());
        let mut refreshed = ChatGptCredential::from_tokens(
            parsed.access_token,
            refresh_token,
            parsed.id_token,
            parsed.expires_in,
        );
        // A refresh may omit the ID token; the account facts carry over.
        refreshed.account_id = refreshed
            .account_id
            .or_else(|| credential.account_id.clone());
        refreshed.email = refreshed.email.or_else(|| credential.email.clone());
        refreshed.plan = refreshed.plan.or_else(|| credential.plan.clone());
        Ok(refreshed)
    }

    /// Sends one Responses request.
    async fn post(
        &self,
        access_token: &str,
        account_id: Option<&str>,
        body: &crate::wire::ResponsesRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = self
            .http
            .post(format!("{}/responses", self.api_base))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::USER_AGENT, user_agent())
            .header("originator", ORIGINATOR)
            .json(body);
        if let Some(account_id) = account_id.filter(|id| !id.is_empty()) {
            request = request.header("chatgpt-account-id", account_id);
        }
        request.send().await.map_err(network_error)
    }

    /// Sends one models request.
    async fn get_models(
        &self,
        access_token: &str,
        account_id: Option<&str>,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = self
            .http
            .get(format!(
                "{}/models?client_version={MODELS_CLIENT_VERSION}",
                self.api_base
            ))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, user_agent())
            .header("originator", ORIGINATOR)
            // A bounded listing call, not a stream.
            .timeout(Duration::from_secs(20));
        if let Some(account_id) = account_id.filter(|id| !id.is_empty()) {
            request = request.header("chatgpt-account-id", account_id);
        }
        request.send().await.map_err(network_error)
    }
}

/// The `User-Agent` every call carries.
fn user_agent() -> String {
    format!("rhema/{}", env!("CARGO_PKG_VERSION"))
}

/// Drains one Responses SSE response to its terminal event,
/// assembling the output and emitting streaming events on the way.
async fn drain_response(
    response: reqwest::Response,
    sink: &mut dyn EventSink,
) -> Result<ModelOutput, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut assembler = StreamAssembler::new();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(network_error)?;
        for payload in decoder.push(&bytes) {
            assembler.push(&payload, sink)?;
        }
        if let Some(err) = decoder.take_error() {
            return Err(ProviderError::Network(err));
        }
    }
    for payload in decoder.finish() {
        assembler.push(&payload, sink)?;
    }
    if let Some(err) = decoder.take_error() {
        return Err(ProviderError::Network(err));
    }
    Ok(assembler.finish())
}

#[async_trait]
impl ModelListing for ChatGptProvider {
    /// The plan's visible chat models (`GET {api}/models`), normalized
    /// by [`normalize_models`]. A 401 refreshes the token and retries
    /// once, the same recovery a turn gets.
    async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let (access, account) = self.token(false).await?;
        let mut response = self.get_models(&access, account.as_deref()).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.can_refresh().await {
            let (access, account) = self.token(true).await?;
            response = self.get_models(&access, account.as_deref()).await?;
        }
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let page: Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed chatgpt models page: {err}"),
            status: None,
        })?;
        Ok(normalize_models(&page))
    }
}

#[async_trait]
impl ModelProvider for ChatGptProvider {
    /// # Errors
    ///
    /// Fails with [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx response (one 401-driven refresh and retry
    /// included), and [`ProviderError::Api`] without one on a malformed
    /// stream event, a mid-stream error, or a failed response.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let mut body = request_body(&req);
        body.prompt_cache_key = Some(self.prompt_cache_key.clone());
        let (access, account) = self.token(false).await?;
        let mut response = self.post(&access, account.as_deref(), &body).await?;

        // A 401 mid-turn means the access token died between the margin
        // check and the call, so it forces one refresh and retry. A second
        // failure is the server's answer, not a refresh problem.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.can_refresh().await {
            let (access, account) = self.token(true).await?;
            response = self.post(&access, account.as_deref(), &body).await?;
        }

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        drain_response(response, sink).await
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the
/// OpenAI error shape's `error.message` (or a plain `detail`/`message`)
/// and carrying the HTTP status structurally for
/// [`ProviderError::is_retryable`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    ProviderError::Api {
        message: format!(
            "chatgpt api error (HTTP {}): {}",
            status.as_u16(),
            error_message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// The message an error body carries, in the shapes this backend and
/// its OAuth endpoints use; the raw body when none is recognizable.
fn error_message(raw: &str) -> String {
    let fallback = raw.trim();
    let Ok(value) = serde_json::from_str::<Value>(fallback) else {
        return if fallback.is_empty() {
            "empty error body".to_owned()
        } else {
            shorten_error(fallback)
        };
    };
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback);
    shorten_error(message)
}

/// Keeps an error short enough to render: an HTML error page from an
/// edge proxy would otherwise fill the screen.
fn shorten_error(message: &str) -> String {
    const MAX_CHARS: usize = 512;
    if message.chars().count() <= MAX_CHARS {
        return message.to_owned();
    }
    let mut shortened: String = message.chars().take(MAX_CHARS).collect();
    shortened.push_str("...");
    shortened
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors
/// cannot be carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::DEFAULT_ISSUER;

    #[test]
    fn error_bodies_map_their_message_in_every_shape() {
        assert_eq!(
            error_message(r#"{"error":{"message":"quota exhausted"}}"#),
            "quota exhausted"
        );
        assert_eq!(
            error_message(r#"{"detail":"Unauthorized"}"#),
            "Unauthorized"
        );
        assert_eq!(
            error_message(r#"{"error":"invalid_grant"}"#),
            "invalid_grant"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "empty error body");
    }

    #[test]
    fn debug_prints_neither_the_access_nor_the_refresh_token() {
        let provider = ChatGptProvider::new(
            ChatGptCredential {
                access_token: "access-secret".into(),
                refresh_token: Some("refresh-secret".into()),
                ..ChatGptCredential::default()
            }
            .to_json(),
        );
        let printed = format!("{provider:?}");
        assert!(!printed.contains("access-secret"), "{printed}");
        assert!(!printed.contains("refresh-secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(printed.contains(DEFAULT_API_BASE), "{printed}");
    }

    #[test]
    fn the_default_issuer_token_url_is_the_one_refreshes_use() {
        assert_eq!(DEFAULT_TOKEN_URL, format!("{DEFAULT_ISSUER}/oauth/token"));
    }
}
