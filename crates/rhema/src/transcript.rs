//! The chat transcript: UI-side lines built from the session history and
//! the engine's streaming events.
//!
//! The transcript is a projection, not state: `CodingAgent::session` stays
//! the single source of truth, and rebuilding from history after a resume
//! or a model switch must render the same lines the live stream produced
//! (within the summary/truncation rules).

use rhema_core::{Content, Message, Role};

use crate::events::OwnedEvent;

/// Width at which a tool-result summary line is cut.
pub const COMMAND_WIDTH: usize = 48;
/// Width at which a tool-call argument echo is cut.
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
    /// A tool invocation and, once it resolved, its outcome — one line.
    ToolCall {
        /// The call's correlation id, matching the result event.
        id: String,
        /// The tool's name.
        name: String,
        /// The accumulated raw JSON argument text; described at render
        /// time.
        args: String,
        /// `None` while the call runs, then the outcome.
        ok: Option<bool>,
        /// The process exit code of command tools; `None` otherwise.
        code: Option<i32>,
    },
    /// A UI notice (model switched, turn declined, ...).
    Notice(String),
}

/// The transcript: lines in stream order.
#[derive(Debug, Default, Clone)]
pub struct Transcript {
    lines: Vec<Line>,
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
                                transcript.lines.push(Line::ToolCall {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    args: call.args.to_string(),
                                    ok: None,
                                    code: None,
                                });
                            }
                            // Results live in Tool-role messages, never in
                            // assistant content.
                            Content::ToolResult(_) => {}
                        }
                    }
                }
                Role::Tool => {
                    for content in &message.content {
                        if let Content::ToolResult(result) = content {
                            if let Some(Line::ToolCall { ok, code, .. }) =
                                transcript.lines.iter_mut().rev().find(|line| {
                                    matches!(line, Line::ToolCall {
                                        id: line_id, ok: None, ..
                                    } if *line_id == result.id)
                                })
                            {
                                *ok = Some(!result.is_error);
                                *code = result.code;
                            }
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
                self.lines.push(Line::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: String::new(),
                    ok: None,
                    code: None,
                });
            }
            OwnedEvent::ToolCallArgsDelta(delta) => {
                if let Some(Line::ToolCall { args, .. }) = self.lines.last_mut() {
                    args.push_str(delta);
                }
            }
            OwnedEvent::ToolResult { id, is_error, code, .. } => {
                // Calls resolve sequentially, but match by id so a late
                // result cannot mark the wrong line; without a match, the
                // oldest unresolved call takes it.
                let has_call = self.lines.iter().rev().any(|line| {
                    matches!(
                        line,
                        Line::ToolCall { id: line_id, ok: None, .. }
                            if *line_id == *id
                    )
                });
                let target = if has_call {
                    self.lines.iter_mut().rev().find(|line| {
                        matches!(
                            line,
                            Line::ToolCall { id: line_id, ok: None, .. }
                                if *line_id == *id
                        )
                    })
                } else {
                    self.lines
                        .iter_mut()
                        .rev()
                        .find(|line| matches!(line, Line::ToolCall { ok: None, .. }))
                };
                if let Some(Line::ToolCall { ok, code: line_code, .. }) = target {
                    *ok = Some(!*is_error);
                    *line_code = *code;
                }
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

/// A human description of a tool call for its transcript line: the built-in
/// tools map to intent plus the argument that matters; unknown tools fall
/// back to their name. Called at render time, so a streaming call's
/// description fills in as its arguments arrive.
pub fn describe(name: &str, args: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
    let arg = |key: &str| parsed.get(key).and_then(|value| value.as_str());
    match name {
        // The shell command renders bare: the `$ ` prefix in the
        // transcript already says what it is.
        "bash" | "run_command" => match arg("command") {
            Some(command) => truncate_chars(command, COMMAND_WIDTH),
            None => "(no command)".to_owned(),
        },
        other => other.to_owned(),
    }
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
            args: json!({ "command": "cat notes.txt" }),
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
            name: "bash".into(),
        });
        transcript.apply(&OwnedEvent::ToolCallArgsDelta("{\"comm".into()));
        transcript.apply(&OwnedEvent::ToolCallArgsDelta("and".into()));
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "wrote notes.txt (3 bytes)\nsecond line".into(),
            is_error: false,
            code: Some(0),
        });
        assert_eq!(
            transcript.lines(),
            &[Line::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: "{\"command".into(),
                ok: Some(true),
                code: Some(0),
            }]
        );
    }

    #[test]
    fn text_after_a_tool_call_opens_a_fresh_assistant_line() {
        let mut transcript = Transcript::new();
        transcript.apply(&OwnedEvent::TextDelta("before".into()));
        transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        });
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "src".into(),
            is_error: false,
            code: Some(0),
        });
        transcript.apply(&OwnedEvent::TextDelta("after".into()));
        assert_eq!(
            transcript.lines(),
            &[
                Line::Assistant("before".into()),
                Line::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: String::new(),
                    ok: Some(true),
                    code: Some(0),
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
            name: "bash".into(),
        });
        streamed.apply(&OwnedEvent::ToolCallArgsDelta(
            "{\"command\":\"cat notes.txt\"}".into(),
        ));
        streamed.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "file body".into(),
            is_error: false,
            code: Some(0),
        });

        let history = vec![
            Message::text(Role::User, "fix it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Text("on it".into()),
                    Content::ToolCall(call("c1", "bash")),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(
                    rhema_core::ToolResult::success("c1", "file body").with_code(0),
                )],
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
            name: "bash".into(),
        });
        transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "exit code 1".into(),
            is_error: true,
            code: Some(1),
        });
        assert_eq!(
            transcript.lines()[0],
            Line::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: String::new(),
                ok: Some(false),
                code: Some(1),
            }
        );
    }

    #[test]
    fn describe_shows_the_bash_command_bare() {
        assert_eq!(describe("bash", r#"{"command": "git status"}"#), "git status");
        assert_eq!(describe("bash", "{}"), "(no command)");
        assert_eq!(describe("bash", "not json"), "(no command)");
        let long = "x".repeat(60);
        assert_eq!(
            describe("bash", &format!(r#"{{"command": "{long}"}}"#)),
            truncate_chars(&long, COMMAND_WIDTH)
        );
    }

    #[test]
    fn describe_falls_back_for_unknown_tools() {
        assert_eq!(describe("mystery", "{}"), "mystery");
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
