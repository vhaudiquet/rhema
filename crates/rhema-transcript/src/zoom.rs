//! The `zoom` tool: the model's way back to any original message.
//!
//! The view summarizes old ranges; zoom opens them. It reads the same
//! mirror the decorator refreshes, so what it returns is the log as the
//! last request carried it — which is every message the model has been
//! shown an id for. Read-only: a `Safe` replay, per the resolver
//! contract.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rhema_core::{ReplaySafety, ToolCall, ToolContext, ToolResult, ToolSpec};

use crate::summary;
use crate::tree::Range;
use crate::view::Transcript;

/// Bytes `zoom(id, 1)` returns before cutting; the cut names the total
/// and the `at` to resume at.
pub const ZOOM_MAX_BYTES: usize = 8 * 1024;

/// The `zoom` resolver over a shared mirror.
#[derive(Debug, Clone)]
pub struct Zoom {
    transcript: Arc<Mutex<Transcript>>,
}

impl Zoom {
    /// The resolver reading `transcript` — the mirror the view
    /// decorator refreshes. Build both sides with [`crate::attach`].
    pub fn new(transcript: Arc<Mutex<Transcript>>) -> Self {
        Self { transcript }
    }

    /// The spec advertised to the model. Its description is the view's
    /// contract: how the lines read and when to zoom.
    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            "zoom",
            "Read older messages of this conversation whole. The \
             <transcript> block in the context summarizes old ranges as \
             `id+n|text` lines; zoom opens them. zoom(id) returns message \
             id whole; zoom(id, n) returns the two halves of the n \
             messages from id on. Long output is cut: pass at (a byte \
             offset) to page on. Before you guess, ask, or re-run a \
             command: if a summary line omits what you need, the \
             original is here.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "index of the range's first message"
                    },
                    "n": {
                        "type": "integer",
                        "description": "how many messages the range covers; 1 returns the message whole",
                        "default": 1
                    },
                    "at": {
                        "type": "integer",
                        "description": "byte offset into the output, to page through a long message",
                        "default": 0
                    }
                },
                "required": ["id"]
            }),
        )
    }

    /// Resolves one zoom call; the in-band form of [`ToolResolver::resolve`],
    /// public for tests and embedders.
    pub fn zoom(&self, call: &ToolCall) -> ToolResult {
        let Some(id) = call.args.get("id").and_then(|v| v.as_u64()) else {
            return ToolResult::failure(&call.id, "zoom needs an integer `id`");
        };
        let n = call
            .args
            .get("n")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let at = call.args.get("at").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let transcript = self.transcript.lock().expect("transcript lock poisoned");
        let len = transcript.len();
        let range = Range { id: id as usize, n };
        if range.id >= len {
            return ToolResult::failure(
                &call.id,
                format!("zoom: message {id} is past the log ({len} messages)"),
            );
        }
        if range.id + range.n > len {
            return ToolResult::failure(
                &call.id,
                format!(
                    "zoom: {id}+{} reaches past the log ({len} messages); \
                     the last message is {}",
                    range.n,
                    len - 1
                ),
            );
        }

        let text = if range.n == 1 {
            let message = transcript.message(range.id).expect("the range was checked");
            summary::render_message(message)
        } else {
            let (first, second) = range.halves();
            let mut out = summary::line_for_range(
                transcript.range(first).expect("the range was checked"),
                first,
            );
            if let Some(second) = second {
                out.push('\n');
                out.push_str(&summary::line_for_range(
                    transcript.range(second).expect("the range was checked"),
                    second,
                ));
            }
            out
        };

        if at >= text.len() {
            return ToolResult::failure(
                &call.id,
                format!(
                    "zoom: `at` {at} is past the end ({} bytes); page from {}",
                    text.len(),
                    text.len().saturating_sub(ZOOM_MAX_BYTES)
                ),
            );
        }
        if text.len() <= ZOOM_MAX_BYTES {
            return ToolResult::success(&call.id, &text[at.min(text.len())..]);
        }
        let start = at.min(text.len());
        let mut end = (start + ZOOM_MAX_BYTES).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        let note = if end < text.len() {
            format!(
                "\n\n[{} of {} bytes; zoom again with at={}]",
                end - start,
                text.len(),
                end
            )
        } else {
            String::new()
        };
        ToolResult::success(&call.id, format!("{}{}", &text[start..end], note))
    }
}

#[async_trait]
impl rhema_core::ToolResolver for Zoom {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![Self::spec()]
    }

    fn replay_safety(&self, name: &str) -> ReplaySafety {
        if name == "zoom" {
            ReplaySafety::Safe
        } else {
            ReplaySafety::Unsafe
        }
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        self.zoom(&call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{Message, Role, ToolResolver};

    fn transcript_with(messages: Vec<Message>) -> Arc<Mutex<Transcript>> {
        let mut transcript = Transcript::new(crate::view::TranscriptSettings::default());
        transcript.observe(&messages);
        Arc::new(Mutex::new(transcript))
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "z1".into(),
            name: "zoom".into(),
            args,
            provider_data: serde_json::Value::Null,
        }
    }

    fn history() -> Vec<Message> {
        vec![
            Message::text(Role::User, "fix the login bug"),
            Message::text(Role::Assistant, "checking auth.rs"),
            Message::text(Role::User, "the session one, not the api key"),
        ]
    }

    #[test]
    fn zoom_one_returns_the_message_whole() {
        let zoom = Zoom::new(transcript_with(history()));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 1 })));
        assert!(!out.is_error);
        assert_eq!(out.content, "checking auth.rs");
    }

    #[test]
    fn zoom_many_returns_two_range_lines() {
        let zoom = Zoom::new(transcript_with(history()));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 2 })));
        assert!(!out.is_error);
        assert!(
            out.content.contains("0+1|user: fix the login bug"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("1+1|assistant: checking auth.rs"),
            "{}",
            out.content
        );
    }

    #[test]
    fn bad_ids_are_in_band_errors() {
        let zoom = Zoom::new(transcript_with(history()));
        let past = zoom.zoom(&call(serde_json::json!({ "id": 3 })));
        assert!(past.is_error);
        assert!(past.content.contains("past the log (3 messages)"));
        let over = zoom.zoom(&call(serde_json::json!({ "id": 1, "n": 9 })));
        assert!(over.is_error);
        let no_id = zoom.zoom(&call(serde_json::json!({ "n": 1 })));
        assert!(no_id.is_error);
    }

    #[test]
    fn a_long_message_pages_at_the_offset() {
        let big = "y".repeat(ZOOM_MAX_BYTES + 500);
        let zoom = Zoom::new(transcript_with(vec![Message::text(Role::User, big)]));
        let first = zoom.zoom(&call(serde_json::json!({ "id": 0 })));
        assert!(first.content.len() <= ZOOM_MAX_BYTES + 100);
        assert!(first.content.contains("at="), "{}", first.content);
        let resume_at: usize = first
            .content
            .split("at=")
            .nth(1)
            .unwrap()
            .trim_end_matches(']')
            .parse()
            .unwrap();
        let rest = zoom.zoom(&call(serde_json::json!({ "id": 0, "at": resume_at })));
        assert!(!rest.is_error, "{:?}", rest);
        assert!(
            rest.content.len() <= 500 + 100,
            "{} bytes",
            rest.content.len()
        );
    }

    #[test]
    fn the_spec_advertises_zoom_and_only_zoom() {
        let zoom = Zoom::new(transcript_with(history()));
        let names: Vec<String> = zoom.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["zoom".to_owned()]);
        let spec = Zoom::spec();
        assert!(spec.description.contains("id+n|text"));
        assert_eq!(zoom.replay_safety("zoom"), ReplaySafety::Safe);
        assert_eq!(zoom.replay_safety("other"), ReplaySafety::Unsafe);
    }

    #[test]
    fn a_text_block_only_history_renders_verbatim() {
        let zoom = Zoom::new(transcript_with(history()));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 2 })));
        assert_eq!(out.content, "the session one, not the api key");
    }
}
