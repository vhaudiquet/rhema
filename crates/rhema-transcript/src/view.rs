//! The request view: the provider decorator that rewrites
//! `req.messages` under a byte budget, and the mirror `zoom` reads.
//!
//! The decorator sits outside everything else on the request path, as
//! paste expansion does: the engine clones the session into the request
//! untouched, the decorator projects it, the session keeps every
//! message (SPEC.md §6). Requests carry the full history, so the
//! decorator's mirror is exactly the log as of the last request — a
//! resumed session rebuilds it wholesale on the first call.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rhema_core::{Content, EventSink, Message, ModelOutput, ModelProvider, ProviderError, Role};

use crate::summary;
use crate::tree::{self, Range};

/// The view's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSettings {
    /// The byte budget a request's messages aim for. The transcript
    /// engages only past it — below, requests pass through
    /// byte-identical — and the current turn may exceed it: the tail is
    /// never cut.
    pub max_view_bytes: usize,
}

impl Default for TranscriptSettings {
    fn default() -> Self {
        Self {
            max_view_bytes: 128_000,
        }
    }
}

/// The mirror: the log as the last request carried it, plus the view
/// built over it. Shared between the decorator (which refreshes it) and
/// the zoom tool (which reads it).
#[derive(Debug)]
pub struct Transcript {
    settings: TranscriptSettings,
    log: Vec<Message>,
}

impl Transcript {
    /// A mirror with no history yet; the first request fills it.
    pub fn new(settings: TranscriptSettings) -> Self {
        Self {
            settings,
            log: Vec::new(),
        }
    }

    /// Folds a request's messages into the log: an extension of what is
    /// already here extends it; anything else — a fresh session, a
    /// resume — replaces it.
    pub fn observe(&mut self, messages: &[Message]) {
        if messages.len() >= self.log.len() && messages[..self.log.len()] == self.log[..] {
            self.log.extend_from_slice(&messages[self.log.len()..]);
        } else {
            self.log = messages.to_vec();
        }
    }

    /// How many messages the log holds.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Message `id`, as `zoom(id, 1)` reads it.
    pub fn message(&self, id: usize) -> Option<&Message> {
        self.log.get(id)
    }

    /// The messages of `range`, as `zoom` reads them; `None` past the
    /// log's end.
    pub fn range(&self, range: Range) -> Option<&[Message]> {
        self.log.get(range.id..range.id + range.n)
    }

    /// The view of `messages`: `None` passes them through unchanged —
    /// below the budget, or when nothing can be represented more
    /// shortly. Above it, the old prefix is covered by structural lines
    /// in one synthetic message, and the recent past continues whole.
    pub fn view(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if summary::messages_bytes(messages) <= self.settings.max_view_bytes {
            return None;
        }
        let len = messages.len();
        // The tail: from the last user message on, always whole. No
        // user message at all — an assistant-only history — and the
        // whole log is up for bounding.
        let tail_start = messages
            .iter()
            .rposition(|message| message.role == Role::User)
            .unwrap_or(len);
        let tail_bytes = summary::messages_bytes(&messages[tail_start..]);
        let mut budget = self.settings.max_view_bytes.saturating_sub(tail_bytes);

        // The recent past walks back whole, newest first; the first
        // message that does not fit ends the band. Older messages go
        // structural even when they would fit: detail fades with age.
        let mut cut = tail_start;
        while cut > 0 {
            let cost = summary::message_bytes(&messages[cut - 1]);
            if cost > budget {
                break;
            }
            budget -= cost;
            cut -= 1;
        }
        let cut = repair_cut(messages, cut);
        let cover = tree::old_cover(cut);
        if cover.is_empty() {
            // Nothing older than the tail: the request is oversized
            // because the turn itself is, and the transcript cannot
            // shrink it.
            return None;
        }

        let mut out = Vec::with_capacity(len - cut + 1);
        out.push(Message::text(Role::User, view_text(messages, &cover)));
        out.extend_from_slice(&messages[cut..]);
        Some(out)
    }
}

/// Moves a cut back until no tool result inside the view answers a call
/// left outside it: the adapters pair `tool_use` with `tool_result` on
/// the wire, so a round is never split. Each pass either returns or
/// moves the cut strictly back, so this terminates.
fn repair_cut(messages: &[Message], mut cut: usize) -> usize {
    loop {
        let mut answered: HashSet<&str> = HashSet::new();
        for message in &messages[cut..] {
            for content in &message.content {
                if let Content::ToolResult(result) = content {
                    answered.insert(result.id.as_str());
                }
            }
        }
        if answered.is_empty() {
            return cut;
        }
        let mut earliest: Option<usize> = None;
        for (i, message) in messages[..cut].iter().enumerate() {
            if message.role != Role::Assistant {
                continue;
            }
            let carries = message.content.iter().any(|content| {
                matches!(content, Content::ToolCall(call) if answered.contains(call.id.as_str()))
            });
            if carries {
                earliest = Some(i);
                break;
            }
        }
        match earliest {
            Some(i) => cut = i,
            None => return cut,
        }
    }
}

/// The synthetic message carrying the structural cover.
fn view_text(messages: &[Message], cover: &[Range]) -> String {
    let mut text = String::from(
        "<transcript>\n\
         The lines below cover this conversation's older messages,\n\
         oldest first. Each reads `id+n|text`: the n messages from index\n\
         id on. zoom(id, 1) returns message id whole; zoom(id, n) opens\n\
         the line into its two halves. The messages after the lines are\n\
         recent; they are whole.\n\n",
    );
    for range in cover {
        text.push_str(&summary::line_for_range(&messages[range.id..], *range));
        text.push('\n');
    }
    text.push_str("</transcript>");
    text
}

/// The transcript as a [`ModelProvider`]: wraps `inner`, rewrites every
/// request's messages through [`Transcript::view`], and refreshes the
/// shared mirror first.
pub struct TranscriptView {
    inner: Arc<dyn ModelProvider>,
    transcript: Arc<Mutex<Transcript>>,
}

impl std::fmt::Debug for TranscriptView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner provider is a trait object and stays opaque.
        f.debug_struct("TranscriptView").finish_non_exhaustive()
    }
}

impl TranscriptView {
    /// The wrapper over `inner`, mirroring into `transcript` — the same
    /// mirror a [`crate::zoom::Zoom`] resolver reads. Build both from
    /// one mirror, or take [`attach`] once it exists.
    pub fn new(inner: Arc<dyn ModelProvider>, transcript: Arc<Mutex<Transcript>>) -> Self {
        Self { inner, transcript }
    }
}

#[async_trait]
impl ModelProvider for TranscriptView {
    async fn complete(
        &self,
        mut req: rhema_core::ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let view = {
            let mut transcript = self.transcript.lock().expect("transcript lock poisoned");
            transcript.observe(&req.messages);
            transcript.view(&req.messages)
        };
        if let Some(messages) = view {
            req.messages = messages;
        }
        self.inner.complete(req, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{ToolCall, ToolResult};
    use std::sync::Mutex as StdMutex;

    /// A big tool result, in kilobytes.
    fn result_bytes(kb: usize) -> String {
        "x".repeat(kb * 1024)
    }

    fn call_round(id: &str, result_kb: usize) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("checking".into()),
                    Content::ToolCall(ToolCall {
                        id: id.into(),
                        name: "bash".into(),
                        args: serde_json::json!({ "command": "make test" }),
                        provider_data: serde_json::Value::Null,
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::success(
                    id,
                    result_bytes(result_kb),
                ))],
            },
        ]
    }

    /// A history over the default budget: many 8 KiB tool rounds, then
    /// a final user message.
    fn long_history(rounds: usize) -> Vec<Message> {
        let mut messages = vec![Message::text(Role::User, "fix the build")];
        for i in 0..rounds {
            messages.extend(call_round(&format!("c{i}"), 8));
        }
        messages.push(Message::text(Role::User, "and now summarize"));
        messages
    }

    #[test]
    fn below_the_budget_the_view_is_none() {
        let transcript = Transcript::new(TranscriptSettings::default());
        let messages = long_history(4); // ~33 KiB
        assert!(transcript.view(&messages).is_none());
    }

    #[test]
    fn above_the_budget_the_old_prefix_is_structural_and_the_tail_whole() {
        let transcript = Transcript::new(TranscriptSettings::default());
        let messages = long_history(30); // ~245 KiB
        let view = transcript.view(&messages).unwrap();

        // One synthetic user message first, tagged, carrying range lines.
        assert_eq!(view[0].role, Role::User);
        let rhema_core::Content::Text(text) = &view[0].content[0] else {
            panic!("text view message");
        };
        assert!(text.starts_with("<transcript>"), "{text}");
        assert!(text.contains("zoom(id, 1)"), "{text}");
        assert!(text.contains("+8|") || text.contains("+16|"), "{text}");

        // The tail — the last user message — is present whole, and the
        // user message of the turn before it too if the budget allowed.
        let last = view.last().unwrap();
        assert_eq!(*last, Message::text(Role::User, "and now summarize"));
        assert!(view.len() < messages.len(), "the view is shorter");
    }

    #[test]
    fn the_cut_never_splits_a_tool_round() {
        let transcript = Transcript::new(TranscriptSettings {
            max_view_bytes: 20_000,
        });
        // user, round c0 (16 KiB), round c1 (16 KiB), user. The budget
        // covers neither whole round, so the cut lands at the user and
        // both rounds go structural — the call and its result together.
        let messages = vec![
            Message::text(Role::User, "go"),
            Message::text(Role::User, "again"),
        ]
        .into_iter()
        .chain(call_round("c0", 16))
        .chain(call_round("c1", 16))
        .chain(std::iter::once(Message::text(Role::User, "done")))
        .collect::<Vec<_>>();
        let view = transcript.view(&messages).unwrap();
        let rendered: Vec<String> = view
            .iter()
            .filter_map(|m| match &m.content[..] {
                [rhema_core::Content::Text(text)] => Some(text.clone()),
                _ => None,
            })
            .collect();
        // Every tool result carried whole has its call carried too:
        // the pairs survive the cut together.
        let carried_calls: HashSet<String> = view
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                rhema_core::Content::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        for message in &view {
            for content in &message.content {
                if let rhema_core::Content::ToolResult(result) = content {
                    assert!(
                        carried_calls.contains(&result.id),
                        "orphan result {} in {rendered:?}",
                        result.id
                    );
                }
            }
        }
    }

    #[test]
    fn repair_cut_moves_back_to_the_calling_assistant() {
        let round = call_round("c7", 1);
        let mut messages = vec![Message::text(Role::User, "go")];
        messages.extend(round);
        messages.push(Message::text(Role::User, "next"));
        // A cut of 2 keeps the result but drops the assistant carrying
        // the call; repair moves it to the assistant, index 1.
        assert_eq!(repair_cut(&messages, 2), 1);
        // A cut before the round stays.
        assert_eq!(repair_cut(&messages, 1), 1);
        assert_eq!(repair_cut(&messages, 0), 0);
    }

    #[test]
    fn the_mirror_extends_on_appends_and_replaces_on_resume() {
        let mut transcript = Transcript::new(TranscriptSettings::default());
        let first = vec![Message::text(Role::User, "one")];
        transcript.observe(&first);
        assert_eq!(transcript.len(), 1);
        let mut second = first.clone();
        second.push(Message::text(Role::Assistant, "two"));
        transcript.observe(&second);
        assert_eq!(transcript.len(), 2);
        // A different history wholesale: a resume into another session.
        let fresh = vec![
            Message::text(Role::User, "a"),
            Message::text(Role::User, "b"),
        ];
        transcript.observe(&fresh);
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript.message(1), Some(&Message::text(Role::User, "b")));
    }

    #[tokio::test]
    async fn the_decorator_rewrites_the_request_and_the_mirror_stays_whole() {
        // Records what it was handed.
        struct Recording(StdMutex<Vec<Vec<Message>>>);
        #[async_trait]
        impl ModelProvider for Recording {
            async fn complete(
                &self,
                req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<ModelOutput, ProviderError> {
                self.0.lock().unwrap().push(req.messages);
                Ok(ModelOutput {
                    message: Message::text(Role::Assistant, "done"),
                    stop_reason: rhema_core::StopReason::EndTurn,
                    usage: rhema_core::Usage::default(),
                })
            }
        }

        let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
        let transcript = Arc::new(Mutex::new(Transcript::new(TranscriptSettings {
            max_view_bytes: 20_000,
        })));
        let view = TranscriptView::new(recording.clone(), transcript.clone());
        let messages = long_history(30);
        let req = rhema_core::ModelRequest {
            system: None,
            messages: messages.clone(),
            tools: Vec::new(),
            params: rhema_core::ChatParams::new("m"),
        };
        let mut sink = NoopSink;
        view.complete(req, &mut sink).await.unwrap();

        let seen = recording.0.lock().unwrap();
        let sent = &seen[0];
        assert!(sent.len() < messages.len());
        assert_eq!(sent.last().unwrap().role, Role::User);
        // The mirror kept every message, none dropped.
        let mirror = transcript.lock().unwrap();
        assert_eq!(mirror.len(), messages.len());
        assert_eq!(mirror.message(0), Some(&messages[0]));
    }

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: rhema_core::Event<'_>) {}
    }
}
