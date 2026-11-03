//! The `zoom` tool: the model's way back to any original message.
//!
//! The view summarizes old ranges; zoom opens them. It reads the same
//! mirror the decorator refreshes, so what it returns is the log as the
//! last request carried it — which is every message the model has been
//! shown an id for. Read-only: a `Safe` replay, per the resolver
//! contract.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use rhema_core::{ReplaySafety, ToolCall, ToolContext, ToolResult, ToolSpec};

use crate::summary;
use crate::tree::Range;
use crate::view::Transcript;

/// Bytes one zoom call returns before cutting; the cut names the
/// total and the `at` to resume at. The same cap and path at every
/// `n`, so paging a range and paging a message are one call shape.
pub const ZOOM_MAX_BYTES: usize = 8 * 1024;

/// The most bytes one zoom call renders before it refuses. A range
/// bigger than this cannot be paged — paging needs the bytes before
/// `at` rendered too — and a narrower `n` is the way. Sixty-four
/// pages: no honest range comes near it, and a model asking for the
/// whole log in one call stops here instead of holding the mirror's
/// lock while it all renders.
const ZOOM_RENDER_CEILING: usize = 64 * ZOOM_MAX_BYTES;

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
             `id+n|text` lines; zoom opens them. zoom(id, n) returns the \
             n messages from id whole, a wide range with each message \
             headed by its id+1| address. Long output is cut: pass at (a \
             byte offset) to page on. Before you guess, ask, or re-run a \
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
                        "description": "how many messages the range covers; the messages come back whole",
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

    /// Resolves one zoom call; the in-band form of
    /// [`rhema_core::ToolResolver::resolve`], public for tests and
    /// embedders.
    ///
    /// `id`, `n`, and `at` arrive from the model and are trusted for
    /// nothing: an out-of-range value is an in-band failure, and `at` is
    /// snapped down to a char boundary before any slice.
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

        let transcript = self
            .transcript
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let len = transcript.len();
        let range = Range { id: id as usize, n };
        if range.id >= len {
            return ToolResult::failure(
                &call.id,
                format!("zoom: message {id} is past the log ({len} messages)"),
            );
        }
        // Saturating: `n` is model-supplied and unbounded; the wrapped
        // sum of a plain add could land inside the log.
        if range.id.saturating_add(range.n) > len {
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

        // Render only what the requested page needs: everything up to
        // one byte past `at + ZOOM_MAX_BYTES`, which is all the offset
        // math reads. A page-1 call stops one message past its page
        // instead of rendering the whole range under the lock.
        let needed = at.saturating_add(ZOOM_MAX_BYTES).saturating_add(1);
        let range_end = range.id + range.n;
        let mut text = String::new();
        let mut covered = range.id;
        for i in range.id..range_end {
            if !text.is_empty() {
                text.push('\n');
            }
            if range.n > 1 {
                // The addresses are the map: a page of content also
                // names the messages it holds, so the model can zoom a
                // neighbor or a narrower range without recomputing.
                text.push_str(&format!("{i}+1|"));
            }
            let message = transcript.message(i).expect("the range was checked");
            text.push_str(&summary::render_message(message));
            covered = i + 1;
            if text.len() >= needed && covered < range_end {
                break;
            }
        }
        let truncated = covered < range_end;
        if truncated && text.len() > ZOOM_RENDER_CEILING {
            return ToolResult::failure(
                &call.id,
                format!(
                    "zoom: {id}+{} renders past the byte budget — \
                     zoom a narrower range (smaller n)",
                    range.n
                ),
            );
        }

        if at >= text.len() {
            if truncated {
                return ToolResult::failure(
                    &call.id,
                    format!(
                        "zoom: `at` {at} lands past what was rendered \
                         ({} bytes); zoom a narrower range (smaller n)",
                        text.len()
                    ),
                );
            }
            let mut msg = format!("zoom: `at` {at} is past the end ({} bytes)", text.len());
            // Only a render bigger than one page has a tail worth
            // pointing at.
            if text.len() > ZOOM_MAX_BYTES {
                msg.push_str(&format!("; page from {}", text.len() - ZOOM_MAX_BYTES));
            }
            return ToolResult::failure(&call.id, msg);
        }
        let start = snapped_back(&text, at);
        if text.len() <= ZOOM_MAX_BYTES {
            return ToolResult::success(&call.id, &text[start..]);
        }
        let mut end = start.saturating_add(ZOOM_MAX_BYTES).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        // The total is only known when the render reached the range's
        // end; a page cut short of it reports its own size instead.
        let note = if end < text.len() {
            if truncated {
                format!("\n\n[{} bytes; zoom again with at={end}]", end - start)
            } else {
                format!(
                    "\n\n[{} of {} bytes; zoom again with at={}]",
                    end - start,
                    text.len(),
                    end
                )
            }
        } else {
            String::new()
        };
        ToolResult::success(&call.id, format!("{}{}", &text[start..end], note))
    }
}

/// The largest char boundary at or below `at` — the model supplies raw
/// byte offsets, and UTF-8 cannot be cut mid-character.
fn snapped_back(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
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
    fn zoom_many_returns_the_messages_whole_and_addressed() {
        let zoom = Zoom::new(transcript_with(history()));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 3 })));
        assert!(!out.is_error);
        assert!(
            out.content.contains("0+1|fix the login bug")
                && out.content.contains("1+1|checking auth.rs")
                && out.content.contains("2+1|the session one, not the api key"),
            "{}",
            out.content
        );
    }

    #[test]
    fn a_wide_range_pages_the_same_way_a_message_does() {
        // Three messages of half the cap each: the range's content
        // cannot fit one page, and the second page begins inside the
        // second message, so the third one's address lands on it.
        let chunk = "y".repeat(ZOOM_MAX_BYTES / 2);
        let transcript = transcript_with(vec![
            Message::text(Role::User, chunk.clone()),
            Message::text(Role::Assistant, chunk.clone()),
            Message::text(Role::User, chunk),
        ]);
        let zoom = Zoom::new(Arc::clone(&transcript));
        let first = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 3 })));
        assert!(first.content.contains("at="), "{}", first.content);
        let resume_at: usize = first
            .content
            .split("at=")
            .nth(1)
            .unwrap()
            .trim_end_matches(']')
            .parse()
            .unwrap();
        let rest = zoom.zoom(&call(
            serde_json::json!({ "id": 0, "n": 3, "at": resume_at }),
        ));
        assert!(!rest.is_error, "{:?}", rest);
        // The last page reaches the range's final message and needs
        // no further cut.
        assert!(rest.content.contains("2+1|"), "{}", rest.content);
        assert!(!rest.content.contains("at="), "{}", rest.content);
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
    fn a_wide_range_renders_one_page_and_names_its_own_size() {
        let line = "x".repeat(600);
        let messages: Vec<Message> = (0..200)
            .map(|i| Message::text(Role::User, format!("msg {i} {line}")))
            .collect();
        let zoom = Zoom::new(transcript_with(messages));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 200 })));
        assert!(!out.is_error, "{:?}", out);
        // One page, not two hundred messages.
        assert!(
            out.content.len() < ZOOM_MAX_BYTES + 2048,
            "{} bytes",
            out.content.len()
        );
        // The render stopped early: the note carries no grand total.
        assert!(
            out.content.contains("zoom again with at="),
            "{}",
            out.content
        );
        assert!(!out.content.contains(" of "), "{}", out.content);
        // Paging continues from the named offset.
        let at: usize = out
            .content
            .rsplit("at=")
            .next()
            .unwrap()
            .trim_end_matches(']')
            .parse()
            .unwrap();
        let rest = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 200, "at": at })));
        assert!(!rest.is_error, "{:?}", rest);
    }

    #[test]
    fn a_range_past_the_ceiling_refuses_with_a_direction() {
        let line = "x".repeat(2000);
        let messages: Vec<Message> = (0..400)
            .map(|i| Message::text(Role::User, format!("msg {i} {line}")))
            .collect();
        let zoom = Zoom::new(transcript_with(messages));
        // Early pages are honest work: the ceiling refuses the deep
        // ones, where honoring `at` means rendering half a megabyte to
        // hand back eight kilobytes.
        let deep = ZOOM_RENDER_CEILING - 2000;
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 400, "at": deep })));
        assert!(
            out.is_error,
            "the deep page does not render: {}",
            &out.content[..200]
        );
        assert!(out.content.contains("narrower range"), "{}", out.content);
        // A single message still reads: the ceiling is for ranges.
        let one = zoom.zoom(&call(serde_json::json!({ "id": 399 })));
        assert!(!one.is_error, "{:?}", one);
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

    #[test]
    fn an_absurd_n_is_an_in_band_error_not_an_overflow() {
        // id=1 with n=u64::MAX wrapped to `id + n == 0` under a plain
        // add, walking past the bounds check into the expects.
        let zoom = Zoom::new(transcript_with(history()));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 1, "n": u64::MAX })));
        assert!(out.is_error);
        assert!(out.content.contains("past the log"), "{}", out.content);
    }

    #[test]
    fn an_at_past_the_end_names_the_content_size() {
        // The render is the content, so the byte count in the error
        // must equal what a successful call over the same range
        // returns, not the size of some condensation of it.
        let zoom = Zoom::new(transcript_with(history()));
        let whole = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 3 })));
        assert!(!whole.is_error);
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "n": 3, "at": 10_000 })));
        assert!(out.is_error);
        let size: usize = out
            .content
            .split('(')
            .nth(1)
            .unwrap()
            .split(" bytes")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(size, whole.content.len(), "{}", out.content);
    }

    #[test]
    fn an_at_inside_a_character_snaps_back_instead_of_panic() {
        // Byte 1 sits inside the two-byte leading `é`: the raw slice
        // used to panic here.
        let zoom = Zoom::new(transcript_with(vec![Message::text(
            Role::User,
            "éa long enough message to hold a paging cut",
        )]));
        let out = zoom.zoom(&call(serde_json::json!({ "id": 0, "at": 1 })));
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.starts_with('é'), "{}", out.content);
    }
}
