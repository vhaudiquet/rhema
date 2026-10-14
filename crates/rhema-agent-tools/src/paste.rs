//! Paste expansion at the provider seam: the model sees plain text, the
//! session keeps the compact chip form.
//!
//! Chat input may carry embedded pastes (`rhema_core::paste`'s encoding,
//! minted by the UIs). Expanding them here — wrapped around whatever
//! `ModelProvider` an agent is built with — means fresh and resumed
//! histories expand alike, and the engine stays ignorant of the encoding.

use std::sync::Arc;

use async_trait::async_trait;
use rhema_core::{
    Content, EventSink, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
};

/// Expands embedded pastes in user text before a request leaves
/// (`rhema_core::paste`): the session carries the compact chip form a
/// UI renders, the model sees only the plain text. Wrapped around
/// every provider the agent is built with, so fresh and resumed
/// histories expand alike. Public so a driver that assembles its own
/// `Agent` — the resident handler — wraps the same seam instead of
/// growing a second expansion.
pub struct ExpandPastes {
    inner: Arc<dyn ModelProvider>,
}

impl ExpandPastes {
    /// The wrapper over `inner`.
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ModelProvider for ExpandPastes {
    async fn complete(
        &self,
        mut req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        for message in &mut req.messages {
            if message.role != Role::User {
                continue;
            }
            for content in &mut message.content {
                if let Content::Text(text) = content {
                    *text = rhema_core::paste::expand(text);
                }
            }
        }
        self.inner.complete(req, sink).await
    }
}
