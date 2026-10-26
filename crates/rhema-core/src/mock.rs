//! Scripted [`MockProvider`] for offline tests and examples: zero network.
//!
//! Replays caller-supplied [`ModelOutput`]s in order and emits the streaming
//! events a real provider would — `TextDelta` for text, `ToolCallStart` plus
//! `ToolCallArgsDelta` for each call — so tests can assert full event
//! sequences (SPEC.md §7 test 2).

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::event::{Event, EventSink};
use crate::output::ModelOutput;
use crate::provider::{ModelProvider, ProviderError};
use crate::request::ModelRequest;

/// A provider that replays scripted outputs in FIFO order. Thread-safe, so
/// it can be shared as `Arc<dyn ModelProvider>`.
pub struct MockProvider {
    replies: Mutex<VecDeque<ModelOutput>>,
}

impl MockProvider {
    /// A mock scripted with `replies`, consumed one per `complete()` call.
    pub fn new(replies: impl IntoIterator<Item = ModelOutput>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
        }
    }

    /// Appends one more reply to the script.
    pub fn push(&self, reply: ModelOutput) {
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(reply);
    }
}

#[async_trait]
impl ModelProvider for MockProvider {
    async fn complete(
        &self,
        _req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let mut replies = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let out = replies.pop_front().ok_or(ProviderError::Exhausted)?;
        drop(replies);

        for content in &out.message.content {
            match content {
                crate::message::Content::Text(text) => sink.emit(Event::TextDelta(text)),
                crate::message::Content::ToolCall(call) => {
                    sink.emit(Event::ToolCallStart {
                        id: &call.id,
                        name: &call.name,
                    });
                    sink.emit(Event::ToolCallArgsDelta {
                        id: &call.id,
                        delta: call.args.to_string().as_str(),
                    });
                }
                crate::message::Content::ToolResult(_) => {}
                // Images ride the message; the event stream carries none.
                crate::message::Content::Image(_) => {}
            }
        }
        Ok(out)
    }
}
