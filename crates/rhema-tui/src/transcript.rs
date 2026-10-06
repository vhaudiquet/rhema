//! The chat transcript: UI-side lines built from the session history and
//! the engine's streaming events.
//!
//! The transcript is a projection, not state: `CodingAgent::session` stays
//! the single source of truth, and rebuilding from history after a resume
//! or a model switch must render the same lines the live stream produced
//! (within the summary/truncation rules).

use std::collections::HashMap;

use rhema_core::{Content, Message, Role};

use crate::events::OwnedEvent;

/// Width at which a tool-result summary line is cut.
pub const SUMMARY_WIDTH: usize = 120;
/// Width at which a tool-call argument echo is cut.
pub const ARGS_WIDTH: usize = 60;
/// Lines of pretty-printed JSON shown in the approval modal before cutting.
pub const ARGS_MAX_LINES: usize = 12;
/// Characters of pretty-printed JSON shown in the approval modal before
/// cutting.
pub const ARGS_MAX_CHARS: usize = 400;

/// One renderable line of the chat transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A user prompt.
    User(String),
    /// Assistant text; streamed deltas merge into these lines.
    Assistant(String),
    /// A tool invocation, rendered `name(args)` on one line.
    ToolCall {
        /// The tool's name.
        name: String,
        /// The accumulated raw JSON argument text, truncated at render
        /// time.
        args: String,
    },
    /// A tool result summary.
    ToolResult {
        /// Whether the call succeeded.
        ok: bool,
        /// First line of the result payload, truncated.
        summary: String,
    },
    /// A UI notice (model switched, turn declined, ...).
    Notice(String),
}

/// The transcript: lines in stream order plus the call-id to tool-name map
/// the result lines need.
#[derive(Debug, Default, Clone)]
pub struct Transcript {
    lines: Vec<Line>,
    calls: HashMap<String, String>,
}

impl Transcript {
    /// An empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the transcript from a session history — the resume /
    /// model-switch path. Must produce the same lines the live stream
    /// would have.
    pub fn from_history(history: &[Message]) -> Self {
        let mut transcript = Self::default();
        for message in history {
            match message.role {
                Role::User => {
                    let text = message
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            Content::Text(text) => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    transcript.push_user(&text);
                }
                Role::Assistant => {
                    for content in &message.content {
                        match content {
                            Content::Text(text) => transcript.append_assistant(text),
                            Content::ToolCall(call) => {
                                transcript.calls.insert(call.id.clone(), call.name.clone());
                                transcript.lines.push(Line::ToolCall {
                                    name: call.name.clone(),
                                    args: call.args.to_string(),
                                });
                            }
                            // Results arrive as Tool-role messages.
                            Content::ToolResult(_) => {}
                        }
                    }
                }
                Role::Tool => {
                    for content in &message.content {
                        if let Content::ToolResult(result) = content {
                            transcript.lines.push(Line::ToolResult {
                                ok: !result.is_error,
                                summary: summarize(&result.content),
                            });
                        }
                    }
                }
                Role::System => {}
            }
        }
        transcript
    }

    /// The lines, in stream order.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Whether nothing has been said yet.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Adds the line for a submitted user prompt.
    pub fn push_user(&mut self, text: &str) {
        self.lines.push(Line::User(text.to_owned()));
    }

    /// Adds a UI notice line.
    pub fn push_notice(&mut self, text: &str) {
        self.lines.push(Line::Notice(text.to_owned()));
    }

    /// Folds one streaming event into the lines.
    pub fn apply(&mut self, event: &OwnedEvent) {
        match event {
            OwnedEvent::StepStart { .. } => {}
            OwnedEvent::TextDelta(delta) => self.append_assistant(delta),
            OwnedEvent::ToolCallStart { id, name } => {
                self.calls.insert(id.clone(), name.clone());
                self.lines.push(Line::ToolCall {
                    name: name.clone(),
                    args: String::new(),
                });
            }
            OwnedEvent::ToolCallArgsDelta(delta) => {
                if let Some(Line::ToolCall { args, .. }) = self.lines.last_mut() {
                    args.push_str(delta);
                }
            }
            OwnedEvent::ToolResult {
                content, is_error, ..
            } => {
                self.lines.push(Line::ToolResult {
                    ok: !*is_error,
                    summary: summarize(content),
                });
            }
            OwnedEvent::StepEnd { .. } => {}
        }
    }

    /// Appends streamed text: deltas continue the last assistant line,
    /// newlines open a new one. Text after a tool-call line opens a fresh
    /// assistant line.
    fn append_assistant(&mut self, delta: &str) {
        for (i, segment) in delta.split('\n').enumerate() {
            if i > 0 {
                self.lines.push(Line::Assistant(String::new()));
            }
            if segment.is_empty() {
                continue;
            }
            if let Some(Line::Assistant(text)) = self.lines.last_mut() {
                text.push_str(segment);
            } else {
                self.lines.push(Line::Assistant(segment.to_owned()));
            }
        }
    }
}

/// First line of a tool result, cut to [`SUMMARY_WIDTH`] columns.
pub fn summarize(content: &str) -> String {
    truncate_chars(content.lines().next().unwrap_or(""), SUMMARY_WIDTH)
}

/// Cuts `text` to at most `max` characters, appending `…` when cut; never
/// splits a multi-byte character.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut end = max - 1;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &text[..end])
}

/// Args as pretty JSON for the approval modal, cut to `max_lines` lines and
/// `max_chars` characters with a marker appended when anything was cut.
pub fn pretty_args(args: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    let mut joined: String = pretty
        .lines()
        .take(ARGS_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if joined.chars().count() > ARGS_MAX_CHARS {
        joined = truncate_chars(&joined, ARGS_MAX_CHARS);
        joined.push_str("\n\u{2026}[truncated]");
    } else if pretty.lines().count() > ARGS_MAX_LINES {
        joined.push_str("\n\u{2026}[truncated]");
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::OwnedEvent;
    use rhema_core::{Role, StopReason, Usage};
    use serde_json::json;

    fn call(id: &str, name: &str) -> rhema_core::ToolCall {
        rhema_core::ToolCall {
            id: id.into(),
            name: name.into(),
            args: json!({ "path": "notes.txt" }),
            provider_data: serde_json::Value::Null,
        }
    }

    #[test]
    fn text_deltas_merge_into_one_assistant_line() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::TextDelta("Hel".into()));
        transcript.apply(&OwnedEvent::TextDelta("lo".into()));
        assert_eq!(transcript.lines(), &[Line::Assistant("Hello".into())]);
    }

    #[test]
    fn newlines_open_new_assistant_lines() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::TextDelta("a\nbb".into()));
        transcript.apply(&OwnedEvent::TextDelta("\n".into()));
        transcript.apply(&OwnedEvent::TextDelta("c".into()));
        assert_eq!(
            transcript.lines(),
            &[
                Line::Assistant("a".into()),
                Line::Assistant("bb".into()),
                Line::Assistant("c".into()),
            ]
        );
    }

    #[test]
    fn tool_call_lines_accumulate_args_and_results_follow() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "write_file".into(),
        });
        transcript.apply(&OwnedEvent::ToolCallArgsDelta("{\"pat".into()));
        transcript.apply(&OwnedEvent::ToolCallArgsDelta("h".into()));
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "wrote notes.txt (3 bytes)\nsecond line".into(),
            is_error: false,
        });
        assert_eq!(
            transcript.lines(),
            &[
                Line::ToolCall {
                    name: "write_file".into(),
                    args: "{\"path".into(),
                },
                Line::ToolResult {
                    ok: true,
                    summary: "wrote notes.txt (3 bytes)".into(),
                },
            ]
        );
    }

    #[test]
    fn text_after_a_tool_call_opens_a_fresh_assistant_line() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::TextDelta("before".into()));
        transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "list_dir".into(),
        });
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "dir: src".into(),
            is_error: false,
        });
        transcript.apply(&OwnedEvent::TextDelta("after".into()));
        assert_eq!(
            transcript.lines(),
            &[
                Line::Assistant("before".into()),
                Line::ToolCall {
                    name: "list_dir".into(),
                    args: String::new(),
                },
                Line::ToolResult {
                    ok: true,
                    summary: "dir: src".into(),
                },
                Line::Assistant("after".into()),
            ]
        );
    }

    #[test]
    fn step_events_are_invisible_but_harmless() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::StepStart { iteration: 0 });
        transcript.apply(&OwnedEvent::StepEnd {
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                cached_input_tokens: 1,
            },
            stop_reason: StopReason::EndTurn,
        });
        assert!(transcript.is_empty());
    }

    #[test]
    fn from_history_rebuilds_what_the_stream_produced() {
        let mut streamed = Transcript::new();
        streamed.push_user("fix it");
        streamed.apply(&OwnedEvent::TextDelta("on it".into()));
        streamed.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "read_file".into(),
        });
        streamed.apply(&OwnedEvent::ToolCallArgsDelta(
            "{\"path\":\"notes.txt\"}".into(),
        ));
        streamed.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "file body".into(),
            is_error: false,
        });

        let history = vec![
            Message::text(Role::User, "fix it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("on it".into()),
                    Content::ToolCall(call("c1", "read_file")),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(rhema_core::ToolResult::success(
                    "c1",
                    "file body",
                ))],
            },
        ];
        let rebuilt = Transcript::from_history(&history);
        assert_eq!(rebuilt.lines(), streamed.lines());
    }

    #[test]
    fn a_failed_result_carries_the_error_mark() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "run_command".into(),
        });
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "exit code 1".into(),
            is_error: true,
        });
        assert_eq!(
            transcript.lines()[1],
            Line::ToolResult {
                ok: false,
                summary: "exit code 1".into(),
            }
        );
    }

    #[test]
    fn summarize_takes_the_first_line_and_cuts_with_an_ellipsis() {
        assert_eq!(summarize("exit code 0\n--- stdout ---"), "exit code 0");
        let cut = summarize(&"x".repeat(300));
        assert_eq!(cut.chars().count(), SUMMARY_WIDTH);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn truncation_stays_on_a_char_boundary() {
        assert_eq!(truncate_chars("héllo wörld", 4), "hé\u{2026}");
    }

    #[test]
    fn pretty_args_cuts_by_lines_and_chars() {
        let args = json!({ "command": "echo hi" });
        assert_eq!(pretty_args(&args), "{\n  \"command\": \"echo hi\"\n}");

        let tall = json!({ "content": value_array() });
        let out = pretty_args(&tall);
        assert!(out.lines().count() <= ARGS_MAX_LINES + 1);
        assert!(out.ends_with("[truncated]"));

        let wide = json!({ "content": "héllo".repeat(300) });
        let out = pretty_args(&wide);
        assert!(out.ends_with("[truncated]"));
        assert!(out.contains("héllo"));
    }

    fn value_array() -> serde_json::Value {
        serde_json::Value::Array((0..100).map(|i| json!(i)).collect())
    }
}
