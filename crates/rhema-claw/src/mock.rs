//! The debug provider (`mock` feature): an endless scripted stand-in so
//! the daemon and a UI can run with zero credentials.
//!
//! [`MockProvider`](rhema_core::MockProvider) replays a fixed script and
//! errors once it runs dry — right for tests, wrong for a daemon a
//! developer chats with all afternoon. This one never dries up: every
//! turn answers with a numbered canned reply, streamed as deltas so a UI
//! exercises the same rendering path a real turn takes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rhema_core::{
    Content, Event, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, Usage, async_trait,
};

/// Answers every request with a canned, streamed reply.
#[derive(Default)]
pub struct MockLoop {
    turns: AtomicU64,
}

impl MockLoop {
    /// The provider, shared as the daemon's backend seam. (Named for what
    /// it returns: a `new` returning `Arc<dyn ModelProvider>` would lie
    /// about constructing `Self`.)
    pub fn provider() -> Arc<dyn ModelProvider> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl ModelProvider for MockLoop {
    async fn complete(
        &self,
        _req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let turn = self.turns.fetch_add(1, Ordering::Relaxed) + 1;
        let text = format!(
            "Mock claw, turn {turn}. No model is attached - every reply \
             is this canned text, streamed in deltas like the real thing."
        );
        // Three char-safe deltas: the UI's streaming path gets exercised,
        // not just the single-frame one.
        let chars: Vec<&str> = {
            let total = text.chars().count();
            let boundary = |n: usize| -> usize {
                text.char_indices()
                    .nth(n)
                    .map(|(i, _)| i)
                    .unwrap_or(text.len())
            };
            vec![
                &text[..boundary(total / 3)],
                &text[boundary(total / 3)..boundary(2 * total / 3)],
                &text[boundary(2 * total / 3)..],
            ]
        };
        for piece in chars {
            sink.emit(Event::TextDelta(piece));
        }
        Ok(ModelOutput {
            message: Message {
                role: Role::Assistant,
                content: vec![Content::Text(text)],
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}
