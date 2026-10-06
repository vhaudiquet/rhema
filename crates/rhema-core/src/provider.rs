//! The model seam: the only boundary to a provider.
//!
//! Transport, auth, retries, and provider wire formats all live behind this
//! trait (future `rhema-provider-*` crates). The engine only ever sees
//! [`ModelRequest`] in and [`ModelOutput`] plus streamed [`Event`]s out.

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
