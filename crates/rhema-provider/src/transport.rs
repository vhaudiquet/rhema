//! The HTTP seam under the provider adapters: one request in, a status
//! and a `Send` byte stream out.
//!
//! Today only the Anthropic adapter speaks its backend's HTTP dialect
//! through this trait, so that backend reaches any runtime the frontends
//! live in; the other adapters hold a private `reqwest` client instead.
//! New adapters should prefer the seam. The contract that matters on the wasm side: JS types never
//! leak through — a host whose fetch stream is backed by JS objects
//! bridges it into plain `Vec<u8>` chunks (its `!Send` handle stays
//! inside a local task; the channel's receiver is plain data and
//! therefore `Send` on single-threaded wasm). Non-2xx responses are not
//! transport errors: the adapter reads the status and shapes its own
//! API error, carrying the status structurally on
//! [`rhema_core::ProviderError::Api`].

use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;

/// One outgoing request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// The HTTP method, uppercase (`GET`, `POST`).
    pub method: String,
    /// The absolute URL.
    pub url: String,
    /// Header pairs in order; duplicates are allowed and sent as given.
    pub headers: Vec<(String, String)>,
    /// The request body, when the method carries one.
    pub body: Option<Vec<u8>>,
    /// Overall time budget for the exchange, where the transport can
    /// apply one. Streams that legitimately run long (a model
    /// generation) send `None` and rely on the transport's stall guard,
    /// if it has one. A host without Rust-side timers ignores this.
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    /// A request with no headers, body, or timeout.
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        }
    }

    /// Appends a header pair.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Serializes `body` as the JSON payload and sets its content type.
    /// The typed body serializes directly — never through a
    /// `serde_json::Value`, whose f64 widening would corrupt f32 wire
    /// fields (a 0.2 temperature would arrive as
    /// 0.20000000298023224).
    ///
    /// # Panics
    ///
    /// Panics when `body` fails to serialize. An adapter's request body
    /// is plain serde data — no map keys that are not strings, nothing
    /// unserializable — so this is a construction-time invariant, not a
    /// runtime path.
    pub fn with_json_body<T: serde::Serialize>(mut self, body: &T) -> Self {
        self.headers
            .push(("content-type".to_owned(), "application/json".to_owned()));
        // An adapter's request body is plain serde data: no map keys
        // that are not strings, nothing that can fail to serialize.
        self.body =
            Some(serde_json::to_vec(body).expect("adapter request bodies are plain serde data"));
        self
    }

    /// Sets the overall timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Why an exchange failed before an HTTP status was readable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The exchange did not finish in time.
    #[error("timed out")]
    Timeout,
    /// Everything else: connect failures, resets, protocol errors.
    #[error("{0}")]
    Other(String),
}

/// A response: the head and the body as an ordered byte-chunk stream.
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response headers, name and value in wire order. Names are
    /// lowercase as HTTP/2 spells them; the same name may appear more
    /// than once. What a consumer reads is its own business — the rate
    /// limit's `retry-after` is the one this stack reads today.
    pub headers: Vec<(String, String)>,
    /// The body, chunks in order.
    pub body: ByteStream,
}

/// A `Send` stream of body chunks. `Send` is the point: the provider
/// adapters' futures must be `Send`, so the stream crossing them must
/// be too — see the module docs for the wasm bridge.
pub type ByteStream = Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send + Unpin>;

/// Sends HTTP requests for the provider adapters.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Sends `request` and returns the response head with the body as a
    /// byte stream. Implementations must not panic and must not retry:
    /// retry policy lives above this seam.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}
