//! The reqwest transport: the native [`Transport`] behind
//! [`AnthropicProvider`](crate::provider::AnthropicProvider)'s default
//! constructor. Owns the two knobs reqwest has and the seam cannot
//! carry — the connect timeout and the per-read stall guard — and the
//! mapping of reqwest's error type onto [`TransportError`], the shape
//! the adapters translate into `ProviderError` (a non-timeout becomes
//! `"network error: ..."` there, which is what makes it retryable).
//! Feature `native`, default on; wasm builds turn it off and bring
//! their own transport over host fetch.

use async_trait::async_trait;
use futures_util::StreamExt;
use rhema_provider::{HttpRequest, HttpResponse, Transport, TransportError};

use std::time::Duration;

/// A `reqwest::Client` speaking the [`Transport`] seam.
#[derive(Clone)]
pub struct ReqwestTransport {
    http: reqwest::Client,
}

impl ReqwestTransport {
    /// A client with the transport knobs the adapters rely on: one
    /// connect attempt bounded at 10 s — a dead network fails fast
    /// instead of hanging the agent step — and a 90 s per-read stall
    /// guard for streams. The stall guard bounds the silence between
    /// chunks, never the total generation time.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(90))
            .user_agent(crate::provider::USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self { http }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a reqwest failure onto the seam's error: a timeout is a timeout,
/// everything else is carried as text (reqwest errors are neither
/// `Clone` nor `Eq`, which [`TransportError`] is).
fn transport_error(err: reqwest::Error) -> TransportError {
    if err.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Other(err.to_string())
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|err| TransportError::Other(err.to_string()))?;
        let mut builder = self.http.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        let response = builder.send().await.map_err(transport_error)?;
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        // The chunk stream stays `Send` here: reqwest's native stream
        // items are plain bytes. (On wasm reqwest's stream is JS-backed
        // and `!Send` — which is exactly why this transport is a
        // native-only feature and the wasm host bridges its own.)
        let stream = response
            .bytes_stream()
            .map(|item| item.map(|bytes| bytes.to_vec()).map_err(transport_error));
        Ok(HttpResponse {
            status,
            headers,
            body: Box::new(stream),
        })
    }
}
