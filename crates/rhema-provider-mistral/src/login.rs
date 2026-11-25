//! The browser sign-in behind [`MistralVibeLogin`]: the way a Mistral
//! Vibe subscription becomes the API key [`crate::MistralProvider`]
//! drives.
//!
//! The flow is Vibe's own PKCE sign-in, run against the console:
//! register a process with the S256 challenge, show the user the
//! `codestral/cli/authenticate` URL, poll the process until the browser
//! finishes, then exchange the completion token and the verifier for an
//! API key. The key is an ordinary Mistral API key scoped to the
//! signed-in plan, so the adapter needs no second wire protocol. A
//! pending process stretches nothing; a `denied`, `expired`, or `error`
//! answer ends the attempt with the server's reason.
//!
//! Cancellation is dropping the task; each attempt holds only its own
//! poll token and verifier, so a dropped flow leaves nothing behind.

use std::time::Duration;

use base64::Engine;
use rand::Rng;
use reqwest::Url;
use sha2::{Digest, Sha256};

use rhema_provider::{InteractiveLogin, LoginInstruction, LoginSink, LoginUpdate};

/// The console that serves Vibe's sign-in routes.
pub const DEFAULT_AUTH_API_BASE_URL: &str = "https://console.mistral.ai/api";

/// How often the process is polled while the browser is open.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How long the flow waits for the browser. The server issues the same
/// ten-minute window in `expires_at`; this is the cap when it does not.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The browser sign-in flow producing the Vibe plan's API key.
#[derive(Debug)]
pub struct MistralVibeLogin {
    auth_api_base_url: String,
    poll_interval: Duration,
    timeout: Duration,
    http: reqwest::Client,
}

/// The start route's response.
#[derive(Debug, Default, serde::Deserialize)]
struct StartResponse {
    #[serde(default)]
    process_id: String,
    #[serde(default)]
    sign_in_url: String,
    #[serde(default)]
    poll_url: String,
}

/// One poll's response.
#[derive(Debug, Default, serde::Deserialize)]
struct PollResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    exchange_token: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// The exchange route's response.
#[derive(Debug, Default, serde::Deserialize)]
struct ExchangeResponse {
    #[serde(default)]
    api_key: Option<String>,
}

/// A failed exchange's body, when it explains a workspace refusal.
#[derive(Debug, Default, serde::Deserialize)]
struct ExchangeError {
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    detail: Option<String>,
}

impl MistralVibeLogin {
    /// A flow against [`DEFAULT_AUTH_API_BASE_URL`].
    pub fn new() -> Self {
        Self {
            auth_api_base_url: DEFAULT_AUTH_API_BASE_URL.to_owned(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
            http: http_client(),
        }
    }

    /// Targets `auth_api_base_url` instead of the production console
    /// (trailing slashes trimmed). A test seam: production code never
    /// calls this.
    pub fn with_auth_api_base_url(mut self, auth_api_base_url: impl Into<String>) -> Self {
        self.auth_api_base_url = auth_api_base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the polling interval. A test seam: production code
    /// never calls this.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Overrides the browser wait. A test seam: production code never
    /// calls this.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Registers one sign-in process with the PKCE challenge.
    async fn start(&self, challenge: &str) -> Result<StartResponse, String> {
        let response = self
            .http
            .post(format!("{}/vibe/sign-in", self.auth_api_base_url))
            .json(&serde_json::json!({
                "code_challenge": challenge,
                "code_challenge_method": "S256",
            }))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|err| format!("could not start Mistral Vibe sign-in: {err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Mistral Vibe sign-in was refused (HTTP {})",
                status.as_u16()
            ));
        }
        let started: StartResponse = response
            .json()
            .await
            .map_err(|err| format!("malformed Mistral Vibe sign-in response: {err}"))?;
        if started.process_id.is_empty()
            || !started
                .process_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("the sign-in response carries no usable process id".to_owned());
        }
        Ok(started)
    }

    /// One poll of the registered process.
    async fn poll(&self, poll_url: &str) -> Result<PollResponse, String> {
        self.same_origin(poll_url, "poll")?;
        let response = self
            .http
            .get(poll_url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|err| format!("could not read Mistral Vibe sign-in status: {err}"))?;
        if response.status() == reqwest::StatusCode::GONE {
            return Ok(PollResponse {
                status: "expired".to_owned(),
                ..PollResponse::default()
            });
        }
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "Mistral Vibe sign-in status was refused (HTTP {})",
                status.as_u16()
            ));
        }
        response
            .json()
            .await
            .map_err(|err| format!("malformed Mistral Vibe sign-in status: {err}"))
    }

    /// Exchanges the completion token and verifier for the plan's key.
    async fn exchange(
        &self,
        process_id: &str,
        token: &str,
        verifier: &str,
    ) -> Result<String, String> {
        let response = self
            .http
            .post(format!(
                "{}/vibe/sign-in/{process_id}/exchange",
                self.auth_api_base_url
            ))
            .json(&serde_json::json!({
                "exchange_token": token,
                "code_verifier": verifier,
            }))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|err| format!("could not finish Mistral Vibe sign-in: {err}"))?;
        let status = response.status();
        let raw = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // The console names the workspace refusal structurally; say
            // what the user can act on rather than a bare status.
            let parsed: ExchangeError = serde_json::from_str(&raw).unwrap_or_default();
            if parsed.error_code == Some(9_004) {
                return Err("your current Mistral workspace cannot be used for Vibe; \
                     sign in again and choose another workspace"
                    .to_owned());
            }
            return Err(format!(
                "Mistral Vibe sign-in could not be completed (HTTP {}): {}",
                status.as_u16(),
                parsed
                    .detail
                    .as_deref()
                    .unwrap_or("the exchange was refused")
            ));
        }
        let exchanged: ExchangeResponse = serde_json::from_str(&raw)
            .map_err(|err| format!("malformed Mistral Vibe exchange response: {err}"))?;
        exchanged
            .api_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| "Mistral Vibe sign-in returned no API key".to_owned())
    }

    /// Whether `url` belongs to the configured console origin. A
    /// server-issued poll URL is followed, so it must not point a
    /// request anywhere else.
    fn same_origin(&self, url: &str, what: &str) -> Result<(), String> {
        let base = Url::parse(&self.auth_api_base_url)
            .map_err(|err| format!("invalid sign-in base URL: {err}"))?;
        let target = Url::parse(url).map_err(|err| format!("invalid sign-in {what} URL: {err}"))?;
        if target.scheme() != base.scheme()
            || target.host_str() != base.host_str()
            || target.port_or_known_default() != base.port_or_known_default()
        {
            return Err(format!(
                "the sign-in {what} URL does not belong to the console"
            ));
        }
        Ok(())
    }

    /// The whole exchange, or the reason it stopped.
    async fn flow(&self, sink: &mut dyn LoginSink) -> Result<String, String> {
        let verifier = random_verifier();
        let challenge = s256_challenge(&verifier);
        let started = self.start(&challenge).await?;
        let sign_in = Url::parse(&started.sign_in_url)
            .map_err(|err| format!("invalid sign-in URL: {err}"))?;
        if !matches!(sign_in.scheme(), "http" | "https") {
            return Err("the sign-in URL is not a web address".to_owned());
        }
        let sign_in_url = sign_in.to_string();
        if started.poll_url.is_empty() {
            return Err("the sign-in response carries no poll URL".to_owned());
        }
        sink.update(LoginUpdate::Instruct(LoginInstruction {
            text: format!(
                "open {} and sign in with your Mistral account to authorize rhema \
                 for your Vibe plan",
                sign_in_url
            ),
            url: Some(sign_in_url),
            code: None,
        }));

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut waiting_reported = false;
        let mut poll_failures = 0u8;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err("Mistral Vibe sign-in timed out".to_owned());
            }
            match self.poll(&started.poll_url).await {
                Ok(polled) => {
                    poll_failures = 0;
                    match polled.status.as_str() {
                        "pending" => {
                            if !waiting_reported {
                                sink.update(LoginUpdate::Waiting(
                                    "waiting for the browser sign-in to finish".to_owned(),
                                ));
                                waiting_reported = true;
                            }
                        }
                        "completed" => {
                            let token = polled.exchange_token.ok_or_else(|| {
                                "the sign-in completed without an exchange token".to_owned()
                            })?;
                            return self.exchange(&started.process_id, &token, &verifier).await;
                        }
                        "expired" => return Err("Mistral Vibe sign-in expired".to_owned()),
                        "denied" => return Err("Mistral Vibe sign-in was denied".to_owned()),
                        "error" => {
                            return Err(polled
                                .message
                                .unwrap_or_else(|| "Mistral Vibe sign-in failed".to_owned()));
                        }
                        other => {
                            return Err(format!(
                                "Mistral Vibe sign-in returned an unknown state: {other}"
                            ));
                        }
                    }
                }
                Err(err) => {
                    // A dropped poll is not a refusal: the console may be
                    // busy. Three in a row is the attempt's failure.
                    poll_failures += 1;
                    if poll_failures >= 3 {
                        return Err(err);
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(self.poll_interval.min(remaining)).await;
        }
    }
}

impl Default for MistralVibeLogin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InteractiveLogin for MistralVibeLogin {
    async fn run(&self, sink: &mut dyn LoginSink) {
        match self.flow(sink).await {
            Ok(api_key) => sink.update(LoginUpdate::Completed(api_key)),
            Err(message) => sink.update(LoginUpdate::Failed(message)),
        }
    }
}

/// The login's HTTP client: the shared bounded connect and name lookup,
/// named as rhema rather than reqwest.
fn http_client() -> reqwest::Client {
    rhema_provider::http::client_builder()
        .user_agent(crate::provider::USER_AGENT)
        .build()
        .expect("the shared client builder is valid")
}

/// A fresh 64-byte PKCE verifier.
fn random_verifier() -> String {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `BASE64URL(SHA256(verifier))`, no padding.
fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_the_s256_of_the_verifier() {
        let verifier = "the-verifier";
        // RFC 7636's ASCII verifier hashes to a stable, URL-safe digest.
        let challenge = s256_challenge(verifier);
        assert!(!challenge.contains('='), "{challenge}");
        assert!(!challenge.contains('+'), "{challenge}");
        assert!(!challenge.contains('/'), "{challenge}");
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn verifiers_are_unique_and_url_safe() {
        let first = random_verifier();
        let second = random_verifier();
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(first.len(), 86);
    }

    #[test]
    fn a_poll_url_outside_the_console_origin_is_refused() {
        let login = MistralVibeLogin::new();
        assert!(
            login
                .same_origin("https://console.mistral.ai/api/vibe/sign-in/poll/x", "poll")
                .is_ok()
        );
        assert!(
            login
                .same_origin("https://evil.example/api/vibe/sign-in/poll/x", "poll")
                .is_err()
        );
    }
}
