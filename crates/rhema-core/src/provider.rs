//! The model seam: the only boundary to a provider.
//!
//! Transport, auth, retries, and provider wire formats all live behind this
//! trait (future `rhema-provider-*` crates). The engine only ever sees
//! [`ModelRequest`] in and [`ModelOutput`] plus streamed
//! [`Event`](crate::Event)s out.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::event::EventSink;
use crate::output::ModelOutput;
use crate::request::ModelRequest;

/// Why a model call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider rejected or failed the call (HTTP error, bad key, ...).
    /// The HTTP status rides structurally, not in the message text: `Some`
    /// when the adapter read one off the response, `None` for a malformed
    /// body or a mid-stream protocol error.
    #[error("provider api error: {message}")]
    Api {
        /// Human-readable description of the failure.
        message: String,
        /// The HTTP status the failure carried, when one was read.
        status: Option<u16>,
    },
    /// A 429 whose response said when to come back. Only a short wait is
    /// retryable (see [`ProviderError::is_retryable`]): a per-minute
    /// token or request window clears in seconds, a usage quota does
    /// not.
    #[error("provider rate limited: {message}")]
    RateLimited {
        /// Human-readable description of the failure.
        message: String,
        /// How long the provider asked the caller to wait.
        retry_after: Duration,
    },
    /// The transport failed before an HTTP status was readable: a connect
    /// failure, a reset, a DNS miss.
    #[error("network error: {0}")]
    Network(String),
    /// The request is malformed before it reaches the wire.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The call did not finish in time.
    #[error("provider request timed out")]
    Timeout,
    /// A scripted provider (the `mock` feature) had no reply left for
    /// the call.
    #[error("mock provider script exhausted")]
    Exhausted,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Whether re-issuing the same request could succeed: a transport
    /// timeout, a network failure, or a server-side status worth asking
    /// again (408 request timeout, any 5xx), read from the status the
    /// adapter carries on [`ProviderError::Api`], never from the message
    /// text. An `Api` without a status is a malformed body or a
    /// mid-stream protocol error, and the other variants are request
    /// defects: none of those a retry fixes.
    ///
    /// A bare 429 is deliberately absent: its wall is a quota window,
    /// not a hiccup, and the retry ladder waits seconds, a 5-hour usage
    /// limit does not clear inside it, so the retries only burn turns
    /// and tokens. The caller sees the error at once (with the
    /// provider's own reset time, where the adapter could read one) and
    /// decides when to come back. The exception is a
    /// [`ProviderError::RateLimited`] whose wait is at most
    /// [`MAX_RATE_LIMIT_WAIT`]: the per-minute windows OpenAI-style
    /// servers enforce, which clear before a retry would be wasted.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Timeout | ProviderError::Network(_) => true,
            ProviderError::RateLimited { retry_after, .. } => *retry_after <= MAX_RATE_LIMIT_WAIT,
            ProviderError::Api {
                status: Some(status),
                ..
            } => matches!(status, 408 | 500..=599),
            ProviderError::Api { status: None, .. }
            | ProviderError::InvalidRequest(_)
            | ProviderError::Exhausted
            | ProviderError::Other(_) => false,
        }
    }
}

/// The longest provider-requested wait a rate limit may ask for and
/// still count as retryable: a minute's window plus slack. Anything
/// longer is a quota, surfaced at once.
pub const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(90);

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

/// A shared provider is a provider: the blanket impl lets a caller
/// holding an `Arc<dyn ModelProvider>` pass it wherever an
/// `impl ModelProvider` is accepted, instead of unwrapping it first.
#[async_trait]
impl<T: ModelProvider + ?Sized> ModelProvider for Arc<T> {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        (**self).complete(req, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_server_failures_are_retryable() {
        assert!(ProviderError::Timeout.is_retryable());
        assert!(ProviderError::Network("connection reset".into()).is_retryable());
        for status in [408, 500, 503, 529] {
            let err = ProviderError::Api {
                message: "gemini api error: overloaded".into(),
                status: Some(status),
            };
            assert!(err.is_retryable(), "{status} retries");
        }
    }

    #[test]
    fn a_short_rate_limit_retries_and_a_long_one_does_not() {
        let short = ProviderError::RateLimited {
            message: "tokens per min".into(),
            retry_after: Duration::from_secs(2),
        };
        assert!(short.is_retryable());
        let long = ProviderError::RateLimited {
            message: "daily quota".into(),
            retry_after: Duration::from_secs(3600),
        };
        assert!(!long.is_retryable());
    }

    #[test]
    fn a_rate_limit_fails_fast() {
        // A quota wall outlives the retry ladder: a 5-hour usage limit
        // does not clear inside a two-second backoff, so the retries
        // would only burn turns and tokens. The caller decides when to
        // come back.
        let err = ProviderError::Api {
            message: "anthropic api error (HTTP 429): rate_limit_error".into(),
            status: Some(429),
        };
        assert!(!err.is_retryable(), "429 does not retry");
    }

    #[test]
    fn request_and_policy_failures_are_not() {
        assert!(!ProviderError::InvalidRequest("no model".into()).is_retryable());
        // Client errors repeat identically; a retry only pays for them.
        for status in [400, 401, 403, 404] {
            let err = ProviderError::Api {
                message: "openai-compatible api error: bad key".into(),
                status: Some(status),
            };
            assert!(!err.is_retryable(), "{status} does not retry");
        }
        // No status carried: a malformed body or a mid-stream protocol
        // error, classified honestly as not retryable.
        assert!(
            !ProviderError::Api {
                message: "malformed SSE chunk: expected `id`".into(),
                status: None,
            }
            .is_retryable()
        );
        assert!(!ProviderError::Exhausted.is_retryable());
        assert!(!ProviderError::Other("wire shape changed".into()).is_retryable());
    }
}
