//! The GitHub device flow behind [`CopilotLogin`]: the one way to turn
//! a Copilot seat into the long-lived GitHub OAuth token
//! [`crate::CopilotProvider`] exchanges.
//!
//! The flow is GitHub's OAuth device grant, run with the public client
//! id of Copilot's official editor integration (`Iv1.b507a08c87ecfe98`)
//! — the device grant requires a registered client, and Copilot seats
//! authorize through that one. Steps: register a device code, show the
//! user the verification URL and user code, then poll the token
//! endpoint at the server-issued interval until the user finishes,
//! with `slow_down` stretching the interval and the server's
//! `expired_token` ending the attempt. The access token GitHub grants
//! does not expire; the short-lived Copilot token it becomes is the
//! provider's business, not the flow's.

use std::time::Duration;

use rhema_core::provider::ProviderError;
use rhema_provider::{LoginInstruction, LoginSink, LoginUpdate};

/// The public OAuth client id of GitHub Copilot's editor integration —
/// the registered client Copilot seats authorize through.
pub const DEFAULT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// The polling interval when the device registration omits one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// The stretch `slow_down` demands of the interval.
const SLOW_DOWN: Duration = Duration::from_secs(5);

/// The OAuth device grant against GitHub, producing the long-lived
/// GitHub token a [`crate::CopilotProvider`] runs on.
pub struct CopilotLogin {
    github_url: String,
    client_id: String,
    poll_interval: Option<Duration>,
    http: reqwest::Client,
}

/// The device registration's response body.
#[derive(Debug, Default, serde::Deserialize)]
struct DeviceCodeResponse {
    #[serde(default)]
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    /// Seconds the device code stays valid; the polling deadline.
    #[serde(default)]
    expires_in: u64,
    /// Seconds between polls.
    #[serde(default)]
    interval: u64,
}

/// The token poll's response body: an `access_token` once authorized,
/// otherwise an `error` naming why not.
#[derive(Debug, Default, serde::Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    error: Option<String>,
}

impl CopilotLogin {
    /// A flow against [`crate::provider::DEFAULT_GITHUB_URL`] with
    /// [`DEFAULT_CLIENT_ID`].
    pub fn new() -> Self {
        Self {
            github_url: crate::provider::DEFAULT_GITHUB_URL.to_owned(),
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            poll_interval: None,
            http: reqwest::Client::new(),
        }
    }

    /// Targets `github_url` instead of
    /// [`crate::provider::DEFAULT_GITHUB_URL`] (trailing slashes
    /// trimmed). A test seam: production code never calls this.
    pub fn with_github_url(mut self, github_url: impl Into<String>) -> Self {
        self.github_url = github_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the server-issued polling interval. A test seam:
    /// production code never calls this, and `slow_down` still stretches
    /// whatever interval is in effect.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = Some(interval);
        self
    }

    /// Registers a device code.
    async fn register(&self) -> Result<DeviceCodeResponse, ProviderError> {
        let response = self
            .http
            .post(format!("{}/login/device/code", self.github_url))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "client_id": self.client_id,
                "scope": "read:user",
            }))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|err| ProviderError::Other(format!("network error: {err}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::Api(format!(
                "copilot device registration failed (HTTP {}): {text}",
                status.as_u16()
            )));
        }
        serde_json::from_str(&text)
            .map_err(|err| ProviderError::Api(format!("malformed device registration: {err}")))
    }

    /// One token poll: the granted token, or the reason it is not yet.
    async fn poll(&self, device_code: &str) -> Result<Result<String, String>, ProviderError> {
        let response = self
            .http
            .post(format!("{}/login/oauth/access_token", self.github_url))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "client_id": self.client_id,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|err| ProviderError::Other(format!("network error: {err}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::Api(format!(
                "copilot token poll failed (HTTP {}): {text}",
                status.as_u16()
            )));
        }
        let parsed: AccessTokenResponse = serde_json::from_str(&text)
            .map_err(|err| ProviderError::Api(format!("malformed token poll: {err}")))?;
        if !parsed.access_token.is_empty() {
            return Ok(Ok(parsed.access_token));
        }
        Ok(Err(parsed.error.unwrap_or_else(|| "unknown error".into())))
    }
}

impl Default for CopilotLogin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl rhema_provider::InteractiveLogin for CopilotLogin {
    async fn run(&self, sink: &mut dyn LoginSink) {
        let registered = match self.register().await {
            Ok(registered) => registered,
            Err(err) => {
                sink.update(LoginUpdate::Failed(err.to_string()));
                return;
            }
        };
        if registered.device_code.is_empty() {
            sink.update(LoginUpdate::Failed(
                "the device registration carries no device code".to_owned(),
            ));
            return;
        }
        sink.update(LoginUpdate::Instruct(LoginInstruction {
            text: format!(
                "open {} in a browser and enter the code to authorize rhema",
                registered.verification_uri
            ),
            url: (!registered.verification_uri.is_empty())
                .then_some(registered.verification_uri.clone()),
            code: (!registered.user_code.is_empty()).then_some(registered.user_code.clone()),
        }));

        let interval = self
            .poll_interval
            .unwrap_or_else(|| match registered.interval {
                0 => DEFAULT_INTERVAL,
                seconds => Duration::from_secs(seconds),
            });
        // The device code's own lifetime bounds the polling; a 0 expires_in
        // cannot happen from GitHub, and a fresh code each attempt keeps a
        // bogus one from spinning forever.
        let deadline =
            std::time::Instant::now() + Duration::from_secs(registered.expires_in.max(1));
        let mut interval = interval;
        loop {
            tokio::time::sleep(interval).await;
            if std::time::Instant::now() >= deadline {
                sink.update(LoginUpdate::Failed(
                    "the device code expired before authorization".to_owned(),
                ));
                return;
            }
            match self.poll(&registered.device_code).await {
                Ok(Ok(access_token)) => {
                    sink.update(LoginUpdate::Completed(access_token));
                    return;
                }
                Ok(Err(error)) => match error.as_str() {
                    "authorization_pending" => {
                        sink.update(LoginUpdate::Waiting(
                            "waiting for you to finish in the browser".to_owned(),
                        ));
                    }
                    "slow_down" => {
                        interval += SLOW_DOWN;
                    }
                    "expired_token" => {
                        sink.update(LoginUpdate::Failed(
                            "the device code expired before authorization".to_owned(),
                        ));
                        return;
                    }
                    "access_denied" => {
                        sink.update(LoginUpdate::Failed(
                            "authorization was denied on GitHub".to_owned(),
                        ));
                        return;
                    }
                    other => {
                        sink.update(LoginUpdate::Failed(format!("GitHub said: {other}")));
                        return;
                    }
                },
                Err(err) => {
                    sink.update(LoginUpdate::Failed(err.to_string()));
                    return;
                }
            }
        }
    }
}
