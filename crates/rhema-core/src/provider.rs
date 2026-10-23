//! The model seam: the only boundary to a provider.
//!
//! Transport, auth, retries, and provider wire formats all live behind this
//! trait (future `rhema-provider-*` crates). The engine only ever sees
//! [`ModelRequest`] in and [`ModelOutput`] plus streamed
//! [`Event`](crate::Event)s out.

use async_trait::async_trait;

use crate::event::EventSink;
use crate::output::ModelOutput;
use crate::request::ModelRequest;

/// Why a model call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider rejected or failed the call (HTTP error, bad key, ...).
    #[error("provider api error: {0}")]
    Api(String),
    /// The request is malformed before it reaches the wire.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The call did not finish in time.
    #[error("provider request timed out")]
    Timeout,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Whether re-issuing the same request could succeed: a transport
    /// timeout, a network failure, or a server-side status worth asking
    /// again (408 request timeout, any 5xx). The adapters format HTTP
    /// failures as `"... (HTTP <status>): ..."` — every `api_error`
    /// helper in the `rhema-provider-*` crates keeps that shape, and
    /// this reads it back. A message without a status is a malformed
    /// body or a mid-stream protocol error, and the other variants are
    /// request defects: none of those a retry fixes.
    ///
    /// A 429 rate limit is deliberately absent: its wall is a quota
    /// window, not a hiccup, and the retry ladder waits seconds — a
    /// 5-hour usage limit does not clear inside it, so the retries only
    /// burn turns and tokens. The caller sees the error at once (with
    /// the provider's own reset time, where the adapter could read one)
    /// and decides when to come back.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Timeout => true,
            ProviderError::Other(msg) => msg.starts_with("network error: "),
            ProviderError::Api(msg) => {
                matches!(http_status(msg), Some(408 | 500..=599))
            }
            ProviderError::InvalidRequest(_) => false,
        }
    }
}

/// The HTTP status an adapter's `api_error` message carries, when it
/// does: the first `(HTTP <status>)` group in the text.
fn http_status(msg: &str) -> Option<u16> {
    let rest = msg.split("(HTTP ").nth(1)?;
    let digits = rest.split(')').next()?;
    digits.parse().ok()
}

/// A model backend: assembles [`ModelOutput`] from one exchange and streams
/// its deltas to `sink` as they arrive.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stream deltas to `sink`; return the assembled final output.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_server_failures_are_retryable() {
        assert!(ProviderError::Timeout.is_retryable());
        assert!(ProviderError::Other("network error: connection reset".into()).is_retryable());
        for status in ["(HTTP 408)", "(HTTP 500)", "(HTTP 503)", "(HTTP 529)"] {
            let err = ProviderError::Api(format!("gemini api error {status}: overloaded"));
            assert!(err.is_retryable(), "{status} retries");
        }
    }

    #[test]
    fn a_rate_limit_fails_fast() {
        // A quota wall outlives the retry ladder: a 5-hour usage limit
        // does not clear inside a two-second backoff, so the retries
        // would only burn turns and tokens. The caller decides when to
        // come back.
        let err = ProviderError::Api("anthropic api error (HTTP 429): rate_limit_error".into());
        assert!(!err.is_retryable(), "429 does not retry");
    }

    #[test]
    fn request_and_policy_failures_are_not() {
        assert!(!ProviderError::InvalidRequest("no model".into()).is_retryable());
        // Client errors repeat identically; a retry only pays for them.
        for status in ["(HTTP 400)", "(HTTP 401)", "(HTTP 403)", "(HTTP 404)"] {
            let err = ProviderError::Api(format!("openai-compatible api error {status}: bad key"));
            assert!(!err.is_retryable(), "{status} does not retry");
        }
        // No status in the message: a malformed body or a mid-stream
        // protocol error, classified honestly as not retryable.
        assert!(!ProviderError::Api("malformed SSE chunk: expected `id`".into()).is_retryable());
        assert!(!ProviderError::Other("wire shape changed".into()).is_retryable());
    }

    #[test]
    fn the_status_reader_takes_the_first_group_and_nothing_else() {
        assert_eq!(http_status("x (HTTP 429): rate limited"), Some(429));
        // An upstream message quoted inside ours may carry its own
        // group; the first is the adapter's.
        assert_eq!(
            http_status("openrouter api error (HTTP 502): upstream said (HTTP 429)"),
            Some(502)
        );
        assert_eq!(http_status("no status here"), None);
        assert_eq!(http_status("(HTTP ): empty"), None);
        assert_eq!(http_status("(HTTP not-a-number)"), None);
    }
}
