//! The extractive line text: what the view says about messages it does
//! not carry whole.
//!
//! Everything here is mechanical — heads and counts read off the
//! messages themselves. No model call belongs to the transcript in v1
//! (spec §7.4): the lines cost no latency and cannot be wrong about
//! what happened, only short. A later compactor replaces exactly the
//! text generators below; the addresses stay.

use rhema_core::{Content, Message, Role};

use crate::tree::Range;

/// Byte cap on one quoted head inside a line.
pub const HEAD_BYTES: usize = 160;

/// Byte cap on one structural line covering a range.
pub const RANGE_HEAD_BYTES: usize = 300;

/// The bytes a message costs on the wire: text, tool-call arguments,
/// and tool-result content, summed. This is the unit the view's budget
/// spends.
pub fn message_bytes(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::Text(text) => text.len(),
            Content::ToolCall(call) => call.name.len() + call.args.to_string().len(),
            Content::ToolResult(result) => result.content.len(),
            Content::Image(image) => image.data.len(),
        })
        .sum()
}

/// `text` on one line, at most `cap` bytes, cut on a char boundary
/// with an ellipsis when longer.
pub fn head(text: &str, cap: usize) -> String {
    let flat = one_line(text);
    if flat.len() <= cap {
        return flat;
    }
    let mut end = cap;
    while end > 0 && !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &flat[..end])
}

/// Newlines and tab runs become single spaces: a line is one line.
pub fn one_line(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut gap = false;
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' || ch == '\t' {
            gap = true;
        } else {
            if gap && !flat.is_empty() {
                flat.push(' ');
            }
            gap = false;
            flat.push(ch);
        }
    }
    flat
}

/// The role's label in a line: `user`, `assistant`, `system`,
/// `result`.
fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "result",
    }
}

/// The aggregate parts of a multi-message range, before addressing.
/// Also the body of the extractive section digest
/// ([`crate::compact::section_digest`]).
pub(crate) fn summarize_range(messages: &[Message], range: Range) -> ((usize, usize), String) {
    let mut user_heads: Vec<String> = Vec::new();
    let mut assistant_heads: Vec<String> = Vec::new();
    let mut calls: Vec<(String, usize)> = Vec::new();
    let mut results = 0usize;
    let mut failures = 0usize;

    let push_head = |heads: &mut Vec<String>, text: &str| {
        if heads.len() < 2 {
            heads.push(head(text, 80));
        }
    };
    for message in &messages[..range.n.min(messages.len())] {
        for content in &message.content {
            match content {
                Content::Text(text) => {
                    if message.role == Role::User {
                        push_head(&mut user_heads, text);
                    } else if message.role == Role::Assistant {
                        push_head(&mut assistant_heads, text);
                    }
                }
                Content::ToolCall(call) => {
                    if let Some(seen) = calls.iter_mut().find(|(name, _)| *name == call.name) {
                        seen.1 += 1;
                    } else {
                        calls.push((call.name.clone(), 1));
                    }
                }
                Content::ToolResult(result) => {
                    results += 1;
                    failures += usize::from(result.is_error);
                }
                // A head quotes text; an image has none.
                Content::Image(_) => {}
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    parts.extend(user_heads.iter().map(|h| format!("user: {h}")));
    if !calls.is_empty() {
        calls.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let named: Vec<String> = calls
            .iter()
            .take(3)
            .map(|(name, count)| format!("{name}\u{d7}{count}"))
            .collect();
        let mut tool_line = if results == 0 {
            // Calls the range opens but its results land elsewhere:
            // say what was called, not that nothing answered.
            format!(
                "{} tool {} ({})",
                calls.iter().map(|(_, n)| n).sum::<usize>(),
                if calls.iter().map(|(_, n)| n).sum::<usize>() == 1 {
                    "call"
                } else {
                    "calls"
                },
                named.join(", ")
            )
        } else {
            format!(
                "{} tool {} ({})",
                results,
                if results == 1 { "result" } else { "results" },
                named.join(", ")
            )
        };
        if calls.len() > 3 {
            tool_line.push_str(", \u{2026}");
        }
        parts.push(tool_line);
    } else if results > 0 {
        parts.push(format!(
            "{} tool {}",
            results,
            if results == 1 { "result" } else { "results" }
        ));
    }
    if failures > 0 {
        parts.push(format!("{failures} failed"));
    }
    parts.extend(assistant_heads.iter().map(|h| format!("assistant: {h}")));
    ((range.id, range.n), parts.join("; "))
}

/// The whole render of one message, as `zoom(id, 1)` returns it: user
/// and assistant text verbatim, tool calls and results spelled out.
/// Uncapped — the zoom pages.
pub fn render_message(message: &Message) -> String {
    let mut out = String::new();
    for content in &message.content {
        match content {
            Content::Text(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            Content::ToolCall(call) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[calls {}({})]", call.name, call.args));
            }
            Content::ToolResult(result) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "[{}: {}]",
                    if result.is_error { "failed" } else { "ok" },
                    result.content
                ));
            }
            Content::Image(image) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[image: {}]", image.media_type.as_str()));
            }
        }
    }
    if out.is_empty() {
        out.push_str(&format!("({} message, empty)", role_label(message.role)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ToolCall;

    #[test]
    fn heads_cut_on_char_boundaries() {
        // Each 'é' is two bytes; a byte cap of 5 lands mid-char.
        let text = "ééééé";
        let h = head(text, 5);
        // Cap 5 lands mid-char; the cut backs up to the boundary and
        // the ellipsis rides on top: "éé" + "…".
        assert_eq!(h, "\u{e9}\u{e9}\u{2026}");
        assert_eq!(head("short", 10), "short");
    }

    #[test]
    fn one_line_flattens_newlines_but_keeps_inner_spaces() {
        assert_eq!(one_line("a\nb\r\nc\td"), "a b c d");
        assert_eq!(one_line("\n\nlead"), "lead");
    }

    #[test]
    fn bytes_count_text_arguments_and_results() {
        let call = Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("running".into()),
                Content::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({ "command": "ls" }),
                    provider_data: serde_json::Value::Null,
                }),
            ],
        };
        // "running" (7) + "bash" (4) + {"command":"ls"} (16)
        assert_eq!(message_bytes(&call), 7 + 4 + 16);
    }

    #[test]
    fn a_single_message_renders_whole_for_zoom() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("step one".into()),
                Content::ToolCall(ToolCall {
                    id: "c9".into(),
                    name: "edit".into(),
                    args: serde_json::json!({ "path": "a.rs" }),
                    provider_data: serde_json::Value::Null,
                }),
            ],
        };
        let out = render_message(&message);
        assert_eq!(out, "step one\n[calls edit({\"path\":\"a.rs\"})]");
    }
}
