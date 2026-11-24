//! HTTP: POST-only Streamable HTTP. The session id captured from the
//! handshake rides every later request with the negotiated
//! `MCP-Protocol-Version`; a `404` under a session id means the
//! session expired and triggers exactly one re-initialize-and-retry.
//! Calls serialize on the session lock: this client's call volume is
//! a shell command at a time, and the session state must stay
//! coherent above all. The queue wait between serialized calls
//! never burns a caller's per-call budget: the clock runs on the
//! wire, not in the queue.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use http_body_util::BodyExt;
use serde_json::Value;

use super::stdio::echoed_numeric;
use super::{
    Client, ClientError, MAX_HTTP_BODY_BYTES, cancelled_line, check_version, envelope_value,
    parse_json_response,
};
use crate::protocol::{self, Incoming, RpcNotification, RpcRequest, method};

/// The pooled HTTP client, HTTPS-capable through rustls.
pub(super) type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    http_body_util::Full<bytes::Bytes>,
>;

/// The HTTP transport's state: the pooled hyper client, the endpoint,
/// config headers, and the negotiated session.
#[derive(Clone)]
pub(super) struct HttpState {
    pub(super) client: HttpClient,
    pub(super) url: hyper::Uri,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) session: Arc<tokio::sync::Mutex<HttpSession>>,
}

/// The session captured at handshake: its id, and the revision in
/// force, which rides `MCP-Protocol-Version` on every later request.
#[derive(Default)]
pub(super) struct HttpSession {
    id: Option<String>,
    version: String,
}

impl Client {
    /// Same for HTTP: the notification POSTs from a detached task, so
    /// a cancelled caller is never blocked on it.
    pub(super) async fn abandon_http(&self, id: u64) {
        if self.take_pending(id).is_some() {
            let http = self.http.clone().expect("http state on http transport");
            let line = cancelled_line(id);
            tokio::spawn(async move {
                let _ = http.post_line(&line).await;
            });
        }
    }

    /// The handshake, HTTP flavor: initialize POST, session capture,
    /// the notification POST.
    pub(super) async fn handshake_http(&self) -> Result<(), ClientError> {
        let http = self.http.as_ref().expect("http state on http transport");
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let line =
            RpcRequest::new(id, method::INITIALIZE, Some(protocol::initialize_params())).to_line();
        let (status, headers, body) = http.post_line(&line).await?;
        if !status.is_success() {
            return Err(ClientError::HttpStatus(status.as_u16()));
        }
        let body = collect_body(body).await?;
        let response = parse_json_response(&body)?;
        let value = envelope_value(response)?;
        let init: protocol::InitializeResult = serde_json::from_value(value)
            .map_err(|_| ClientError::Protocol("initialize result malformed"))?;
        check_version(self.transport, &init.protocol_version)?;

        let mut session = http.session.lock().await;
        session.id = headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        session.version = init.protocol_version;
        drop(session);

        let notification = RpcNotification::new(method::INITIALIZED, None).to_line();
        let (status, _headers, _body) = http.post_line(&notification).await?;
        // A notification POST reads as success when answered 2xx with
        // no body (spec §6: 202 Accepted).
        if !status.is_success() {
            return Err(ClientError::HttpStatus(status.as_u16()));
        }
        Ok(())
    }

    /// One HTTP request-response exchange, with the 404
    /// re-initialize-and-retry-once dance (spec §6). `timeout` bounds
    /// the exchange only, never the queue wait: calls serialize on
    /// the session lock by design (module doc), and time spent
    /// waiting behind a slower predecessor is the queue's, not this
    /// call's. The clock starts the moment the lock is held, so a
    /// queued call keeps its whole budget on the wire and a timeout
    /// always means the wire. Releasing the lock before the response
    /// body arrives would also let a queued call through sooner, but
    /// every session mutation around the lock (the 404
    /// re-initialize here, the cancellation POST from
    /// `abandon_http`) assumes one request in flight per session;
    /// keeping the hold and moving the clock start is the
    /// smaller-risk change.
    pub(super) async fn http_exchange(
        &self,
        http: &HttpState,
        id: u64,
        line: &str,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        // Take the lock outside the timer: this wait costs the
        // caller nothing. From here the exchange runs on the clock.
        let first = http.session.clone().lock_owned().await;
        let exchange = self.http_exchange_attempts(http, id, line, Some(first));
        match tokio::time::timeout(timeout, exchange).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ClientError::Timeout),
        }
    }

    /// The exchange attempts, entered with the first session guard in
    /// hand. The 404 dance drops the guard around the re-initialize
    /// (which takes the lock itself); the second attempt re-locks
    /// while the same budget keeps running.
    async fn http_exchange_attempts(
        &self,
        http: &HttpState,
        id: u64,
        line: &str,
        mut held: Option<tokio::sync::OwnedMutexGuard<HttpSession>>,
    ) -> Result<Value, ClientError> {
        for _attempt in 0..2 {
            let session = match held.take() {
                Some(guard) => guard,
                None => http.session.clone().lock_owned().await,
            };
            let had_session = session.id.is_some();
            let (status, headers, body) = http.post_line_with(line, &session).await?;
            drop(session);
            if status.as_u16() == 404 && had_session {
                // Session expired: re-initialize exactly once, retry.
                self.handshake_http().await?;
                continue;
            }
            let content_type = headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if content_type.starts_with("text/event-stream") {
                return sse_answer(body, id).await;
            }
            if !status.is_success() {
                return Err(ClientError::HttpStatus(status.as_u16()));
            }
            let body = collect_body(body).await?;
            if body.is_empty() {
                // 202 with no body means "response on another
                // channel"; this client opens no such channel.
                return Err(ClientError::Protocol(
                    "server accepted the request without answering it",
                ));
            }
            let response = parse_json_response(&body)?;
            return envelope_value(response);
        }
        Err(ClientError::Protocol(
            "re-initialization did not recover the session",
        ))
    }
}

/// Drains a response body into its bytes, refusing one over
/// [`MAX_HTTP_BODY_BYTES`]: HTTP has no framing cap of its own, and a
/// configured server is still a server that can misbehave.
async fn collect_body(body: hyper::body::Incoming) -> Result<bytes::Bytes, ClientError> {
    let mut body = body;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| ClientError::Http(err.to_string()))?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
            return Err(ClientError::Protocol(
                "http response body exceeded the 4 MiB cap",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(bytes))
}

impl HttpState {
    /// One POST with the transport's fixed, session, and config
    /// headers. Takes the session lock itself (handshake path).
    async fn post_line(
        &self,
        line: &str,
    ) -> Result<(hyper::StatusCode, hyper::HeaderMap, hyper::body::Incoming), ClientError> {
        let session = self.session.lock().await;
        self.post_line_with(line, &session).await
    }

    /// The POST with an already-held session guard.
    async fn post_line_with(
        &self,
        line: &str,
        session: &HttpSession,
    ) -> Result<(hyper::StatusCode, hyper::HeaderMap, hyper::body::Incoming), ClientError> {
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(self.url.clone())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(id) = &session.id {
            builder = builder.header("mcp-session-id", id.clone());
        }
        if !session.version.is_empty() {
            builder = builder.header("mcp-protocol-version", session.version.clone());
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.clone(), value.clone());
        }
        let request = builder
            .body(http_body_util::Full::new(bytes::Bytes::from(
                line.to_owned(),
            )))
            .map_err(|err| ClientError::Http(format!("request build: {err}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|err| ClientError::Http(err.to_string()))?;
        let (parts, body) = response.into_parts();
        Ok((parts.status, parts.headers, body))
    }
}

/// Waits for the request's answer inside an SSE stream: frames
/// accumulate, `data:` lines build events, and the event carrying the
/// request's id wins. Other events pass by.
async fn sse_answer(body: hyper::body::Incoming, id: u64) -> Result<Value, ClientError> {
    let mut body = body;
    let mut buffer: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut data_bytes: usize = 0;
    let mut total: usize = 0;
    loop {
        let chunk = match body.frame().await {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(chunk) => chunk,
                // A trailer, not data: skip it.
                Err(_frame) => continue,
            },
            Some(Err(err)) => return Err(ClientError::Http(err.to_string())),
            None => {
                return Err(ClientError::Protocol(
                    "the SSE stream closed before the response arrived",
                ));
            }
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_HTTP_BODY_BYTES {
            return Err(ClientError::Protocol(
                "http response body exceeded the 4 MiB cap",
            ));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=pos).collect();
            line.pop(); // the newline
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                // Event boundary: dispatch what data lines gathered.
                if let Some(answer) = event_answer(&data_lines, id)? {
                    return Ok(answer);
                }
                data_lines.clear();
                data_bytes = 0;
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                data_bytes = data_bytes.saturating_add(data.len());
                if data_bytes > MAX_HTTP_BODY_BYTES {
                    return Err(ClientError::Protocol("an SSE event exceeded the 4 MiB cap"));
                }
                data_lines.push(data.to_owned());
            }
            // Other SSE fields (`event:`, `id:`, `retry:`) pass by.
        }
    }
}

/// Joins an event's data lines and, when the event carries the wanted
/// response, returns its value. Notifications riding the stream, and
/// responses to other ids, pass by.
fn event_answer(data_lines: &[String], id: u64) -> Result<Option<Value>, ClientError> {
    if data_lines.is_empty() {
        return Ok(None);
    }
    let joined = data_lines.join("\n");
    let incoming: Incoming = serde_json::from_str(&joined)
        .map_err(|_| ClientError::Protocol("an SSE event was not valid JSON-RPC"))?;
    let Incoming::Response(response) = incoming else {
        return Ok(None);
    };
    if echoed_numeric(&response.id) == Some(id) {
        envelope_value(response).map(Some)
    } else {
        Ok(None)
    }
}
