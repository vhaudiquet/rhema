//! The browser OAuth flow behind [`ChatGptLogin`]: the one way to turn a
//! ChatGPT subscription into the credential
//! [`crate::ChatGptProvider`] refreshes and drives the backend with.
//!
//! The flow is OpenAI's OAuth authorization-code grant with PKCE, run
//! against the public client id of the Codex CLI, the registered
//! client ChatGPT subscriptions authorize through. Steps: generate the
//! PKCE verifier and state, bind a loopback callback listener on the
//! registered port, show the user the authorization URL (and try to
//! open it), wait for the browser's redirect, validate the state,
//! exchange the code for tokens, and report the credential as one JSON
//! string.
//!
//! Cancellation is dropping the task. The listener and any held
//! callback close with it. The stable port ([`CALLBACK_PORT`], falling
//! back to [`CALLBACK_FALLBACK_PORT`]) is what the client's redirect
//! allow-list names; a browser on another machine needs that port
//! forwarded (`ssh -L 1455:localhost:1455 …`), and the instruction says
//! so.

use std::time::Duration;

use base64::Engine;
use rand::RngCore;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use rhema_core::provider::ProviderError;
use rhema_provider::{InteractiveLogin, LoginInstruction, LoginSink, LoginUpdate};

use crate::credential::ChatGptCredential;

/// The public client id of the Codex CLI, the registered OAuth client
/// ChatGPT subscriptions authorize through.
pub const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The issuer's origin.
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";

/// The issuer's token endpoint.
pub const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The scopes the flow asks for: identity claims and an offline refresh
/// token.
pub const DEFAULT_SCOPE: &str = "openid profile email offline_access";

/// The loopback callback port the client's redirect allow-list names.
pub const CALLBACK_PORT: u16 = 1455;

/// The registered fallback when [`CALLBACK_PORT`] is taken.
pub const CALLBACK_FALLBACK_PORT: u16 = 1457;

/// The callback path.
const CALLBACK_PATH: &str = "/auth/callback";

/// How long the flow waits for the browser.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The OAuth authorization-code flow with PKCE, producing the JSON
/// credential a [`crate::ChatGptProvider`] runs on.
#[derive(Debug)]
pub struct ChatGptLogin {
    issuer: String,
    client_id: String,
    token_url: String,
    /// `None` tries [`CALLBACK_PORT`] then
    /// [`CALLBACK_FALLBACK_PORT`]; `Some(0)` binds an ephemeral port.
    callback_port: Option<u16>,
    open_browser: bool,
    timeout: Duration,
    http: reqwest::Client,
}

impl ChatGptLogin {
    /// A flow against [`DEFAULT_ISSUER`] with [`DEFAULT_CLIENT_ID`].
    pub fn new() -> Self {
        Self {
            issuer: DEFAULT_ISSUER.to_owned(),
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            token_url: DEFAULT_TOKEN_URL.to_owned(),
            callback_port: None,
            open_browser: true,
            timeout: DEFAULT_TIMEOUT,
            http: http_client(),
        }
    }

    /// Targets `issuer` instead of [`DEFAULT_ISSUER`] (trailing
    /// slashes trimmed). The token endpoint follows the issuer unless
    /// [`Self::with_token_url`] already named another. A test seam:
    /// production code never calls this.
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into().trim_end_matches('/').to_owned();
        if self.token_url == DEFAULT_TOKEN_URL {
            self.token_url = format!("{}/oauth/token", self.issuer);
        }
        self
    }

    /// Authorizes with `client_id` instead of [`DEFAULT_CLIENT_ID`]. A
    /// test seam: production code never calls this.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = client_id.into();
        self
    }

    /// Exchanges at `token_url` instead of [`DEFAULT_TOKEN_URL`]. A
    /// test seam: production code never calls this.
    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Binds `port` instead of trying the registered pair; `0` binds an
    /// ephemeral one. A test seam: production code never calls this.
    pub fn with_callback_port(mut self, port: u16) -> Self {
        self.callback_port = Some(port);
        self
    }

    /// Whether the flow opens the authorization URL in the platform's
    /// browser. On by default; a test or a headless host turns it off.
    pub fn with_open_browser(mut self, open: bool) -> Self {
        self.open_browser = open;
        self
    }

    /// Bounds the wait for the browser. A test seam: production never
    /// calls this.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Binds the callback listener.
    async fn bind(&self) -> Result<TcpListener, String> {
        if let Some(port) = self.callback_port {
            return TcpListener::bind(("127.0.0.1", port))
                .await
                .map_err(|err| format!("cannot listen on 127.0.0.1:{port}: {err}"));
        }
        match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
            Ok(listener) => Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                TcpListener::bind(("127.0.0.1", CALLBACK_FALLBACK_PORT))
                    .await
                    .map_err(|fallback| {
                        format!(
                            "cannot listen on 127.0.0.1:{CALLBACK_PORT} or \
                             127.0.0.1:{CALLBACK_FALLBACK_PORT} (another sign-in \
                             may be running): {fallback}"
                        )
                    })
            }
            Err(err) => Err(format!("cannot listen on 127.0.0.1:{CALLBACK_PORT}: {err}")),
        }
    }

    /// Exchanges an authorization code for the credential.
    async fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<ChatGptCredential, ProviderError> {
        let response = self
            .http
            .post(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", self.client_id.as_str()),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
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
        let parsed: crate::credential::TokenResponse =
            serde_json::from_str(&text).map_err(|err| ProviderError::Api {
                message: format!("malformed ChatGPT token response: {err}"),
                status: None,
            })?;
        if parsed.access_token.is_empty() {
            return Err(ProviderError::Api {
                message: "the ChatGPT token response carries no access token".to_owned(),
                status: None,
            });
        }
        Ok(ChatGptCredential::from_tokens(
            parsed.access_token,
            parsed.refresh_token,
            parsed.id_token,
            parsed.expires_in,
        ))
    }

    /// The flow proper; every failure is one `Failed` update.
    async fn flow(&self, sink: &mut dyn LoginSink) -> Result<(), String> {
        let listener = self.bind().await?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("cannot read the callback address: {err}"))?
            .port();
        let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
        let pkce = Pkce::generate();
        let state = random_token();
        let url = authorize_url(
            &self.issuer,
            &self.client_id,
            &redirect_uri,
            &pkce.challenge,
            &state,
        )?;
        sink.update(LoginUpdate::Instruct(LoginInstruction {
            text: format!(
                "open this URL in a browser and sign in with ChatGPT. If the \
                 browser runs on another machine, forward port {port} back \
                 here first, for example ssh -L {port}:localhost:{port} <host>"
            ),
            url: Some(url.clone()),
            code: None,
        }));
        if self.open_browser {
            open_browser(&url);
        }
        sink.update(LoginUpdate::Waiting(
            "waiting for the browser to finish signing in".to_owned(),
        ));

        let (code, mut callback) =
            tokio::time::timeout(self.timeout, wait_for_callback(listener, &state))
                .await
                .map_err(|_| "the sign-in timed out before the browser returned".to_owned())??;

        match self.exchange(&code, &redirect_uri, &pkce.verifier).await {
            Ok(credential) => {
                respond(
                    &mut callback,
                    200,
                    "Signed in to ChatGPT",
                    "You can close this tab and return to rhema.",
                )
                .await;
                sink.update(LoginUpdate::Completed(credential.to_json()));
                Ok(())
            }
            Err(err) => {
                respond(&mut callback, 500, "Sign-in failed", &err.to_string()).await;
                Err(err.to_string())
            }
        }
    }
}

impl Default for ChatGptLogin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl InteractiveLogin for ChatGptLogin {
    async fn run(&self, sink: &mut dyn LoginSink) {
        if let Err(message) = self.flow(sink).await {
            sink.update(LoginUpdate::Failed(message));
        }
    }
}

/// The login's HTTP client: a bounded connect and the client's own
/// user agent, so the token endpoint sees rhema rather than reqwest.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(format!("rhema/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("a client builder with only timeouts cannot fail")
}

/// The PKCE pair: the secret verifier and the S256 challenge the
/// authorization URL carries.
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// A fresh 64-byte verifier and its challenge.
    fn generate() -> Self {
        let mut bytes = [0u8; 64];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let challenge = s256_challenge(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// `BASE64URL(SHA256(verifier))`, no padding.
fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// A 32-byte URL-safe token, used as the OAuth `state` and the
/// conversation's prompt-cache key.
pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Builds the authorization URL.
fn authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<String, String> {
    let endpoint = format!("{}/oauth/authorize", issuer.trim_end_matches('/'));
    let mut url = Url::parse(&endpoint)
        .map_err(|err| format!("invalid ChatGPT authorization endpoint {endpoint}: {err}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", DEFAULT_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true");
    Ok(url.into())
}

/// Waits for the browser's callback, ignoring unrelated requests (a
/// favicon fetch, say).
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<(String, TcpStream), String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|err| format!("the callback listener failed: {err}"))?;
        let head = match tokio::time::timeout(Duration::from_secs(10), read_head(&mut stream)).await
        {
            Ok(Ok(head)) => head,
            // A connection that says nothing is not the browser's
            // redirect; keep waiting.
            _ => continue,
        };
        let Some(target) = request_target(&head) else {
            respond(
                &mut stream,
                400,
                "Bad request",
                "The callback was malformed.",
            )
            .await;
            continue;
        };
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != CALLBACK_PATH {
            respond(
                &mut stream,
                404,
                "Not found",
                "This is not the rhema sign-in callback.",
            )
            .await;
            continue;
        }
        let params = parse_query(query);
        if params.get("state").map(String::as_str) != Some(expected_state) {
            respond(
                &mut stream,
                400,
                "Sign-in rejected",
                "The callback state did not match this sign-in attempt.",
            )
            .await;
            return Err("the OAuth callback state did not match this sign-in attempt".to_owned());
        }
        if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or_default();
            respond(
                &mut stream,
                400,
                "Sign-in refused",
                &format!("OpenAI refused the authorization: {error} {description}"),
            )
            .await;
            return Err(format!(
                "ChatGPT sign-in was refused: {error}{}",
                if description.is_empty() {
                    String::new()
                } else {
                    format!(" ({description})")
                }
            ));
        }
        let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
            respond(
                &mut stream,
                400,
                "Sign-in failed",
                "The callback carried no authorization code.",
            )
            .await;
            return Err("the OAuth callback carried no authorization code".to_owned());
        };
        return Ok((code.clone(), stream));
    }
}

/// Reads an HTTP request head, bounded.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    const MAX_HEAD: usize = 16 * 1024;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() >= MAX_HEAD {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// The request target (the second token of the request line).
fn request_target(head: &str) -> Option<&str> {
    head.lines().next()?.split_whitespace().nth(1)
}

/// Parses a query string into percent-decoded pairs, last value winning.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(name), percent_decode(value));
    }
    params
}

/// Percent-decodes a query component (`+` reads as a space).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        decoded.push(high * 16 + low);
                        index += 3;
                        continue;
                    }
                    _ => {
                        decoded.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// One hexadecimal digit's value.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Writes a small terminal HTML page.
async fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;\
         padding:0 1rem;color:#1f2328}}h1{{font-size:1.4rem}}</style></head>\
         <body><h1>{}</h1><p>{}</p></body></html>",
        html_escape(title),
        html_escape(title),
        html_escape(message)
    );
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Escapes text for the callback page.
fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Maps a non-2xx token response onto [`ProviderError::Api`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let detail = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
                .or_else(|| {
                    value
                        .get("error_description")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_owned)
        })
        .unwrap_or_else(|| raw.trim().to_owned());
    let detail: String = if detail.chars().count() > 512 {
        let mut short: String = detail.chars().take(512).collect();
        short.push_str("...");
        short
    } else {
        detail
    };
    ProviderError::Api {
        message: format!(
            "ChatGPT sign-in failed (HTTP {}): {detail}",
            status.as_u16()
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`].
fn network_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(err.to_string())
    }
}

/// Best-effort browser launch; a headless host just shows the URL.
#[cfg(target_os = "macos")]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Best-effort browser launch; a headless host just shows the URL.
#[cfg(target_os = "windows")]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Best-effort browser launch; a headless host just shows the URL.
#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_s256_challenge_matches_the_rfc_vector() {
        // RFC 7636, appendix B.
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_generated_pair_is_url_safe_and_padded_free() {
        let pkce = Pkce::generate();
        assert!(pkce.verifier.len() >= 43);
        assert!(!pkce.verifier.contains('='));
        assert!(!pkce.challenge.contains('='));
        assert_eq!(pkce.challenge, s256_challenge(&pkce.verifier));
    }

    #[test]
    fn the_authorization_url_carries_pkce_state_and_the_callback() {
        let url = authorize_url(
            "https://auth.openai.com/",
            "client",
            "http://localhost:1455/auth/callback",
            "challenge",
            "state",
        )
        .expect("the URL parses");
        let parsed = Url::parse(&url).expect("the URL round-trips");
        assert_eq!(parsed.path(), "/oauth/authorize");
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(params.get("client_id").map(String::as_str), Some("client"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state"));
        assert_eq!(
            params.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(params.get("scope").map(String::as_str), Some(DEFAULT_SCOPE));
    }

    #[test]
    fn query_values_percent_decode_and_plus_reads_as_space() {
        let params = parse_query("code=a%2Fb%2Bc&state=x+y&empty=&bare");
        assert_eq!(params.get("code").map(String::as_str), Some("a/b+c"));
        assert_eq!(params.get("state").map(String::as_str), Some("x y"));
        assert_eq!(params.get("empty").map(String::as_str), Some(""));
        assert_eq!(params.get("bare").map(String::as_str), Some(""));
    }

    #[test]
    fn the_request_target_is_the_second_token() {
        let head = "GET /auth/callback?code=x HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(request_target(head), Some("/auth/callback?code=x"));
        assert_eq!(request_target("nonsense"), None);
    }

    #[test]
    fn the_page_escapes_what_it_renders() {
        assert_eq!(html_escape("<a&b>"), "&lt;a&amp;b&gt;");
    }
}
