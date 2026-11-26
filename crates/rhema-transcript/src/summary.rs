//! The extractive line text: what the view says about messages it does
//! not carry whole.
//!
//! Everything here is mechanical, heads and counts read off the
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

/// Byte cap on one user message carried whole in a digest. A user's
/// instruction is the one thing the block must not paraphrase, so it
/// stands as written up to this size. Past it the head stands and an
/// explicit marker states the message's full length, so a cut is
/// visible and worth a `zoom`.
pub const USER_BYTES: usize = 8 * 1024;

/// The bytes one image costs the budget. A provider tokenizes an image
/// by its pixels, not by its base64 length: a 228 KB screenshot is
/// about two thousand tokens, near the top of the range. Charging the
/// encoded length would count that screenshot as 57 KB of text, and an
/// image-heavy turn would flush the block over bytes compaction can
/// never remove. One image is charged as one small message.
pub const IMAGE_BYTES: usize = 1024;

/// The bytes a content kind this build does not know costs: its
/// serialized size, so a part added to [`Content`] later enters the
/// budget at what it sends instead of for free. The arms below price
/// the kinds they know; this one keeps an unknown kind honest.
pub fn unknown_bytes(content: &Content) -> usize {
    serde_json::to_string(content).map_or(0, |json| json.len())
}

/// The bytes a message costs the budget: text, tool-call arguments,
/// and tool-result content, summed, with each image at [`IMAGE_BYTES`]
/// instead of its encoded length. This is the unit the view's budget
/// spends.
pub fn message_bytes(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::Text(text) => text.len(),
            Content::ToolCall(call) => call.name.len() + call.args.to_string().len(),
            Content::ToolResult(result) => result.content.len() + result.images.len() * IMAGE_BYTES,
            Content::Image(_) => IMAGE_BYTES,
            _ => unknown_bytes(content),
        })
        .sum()
}

/// The tokens one image stands for: a vision price, not a text
/// length. A flat figure keeps an image-heavy turn from reading as
/// millions of tokens; the footer's context gauge prices an image the
/// same way.
pub const IMAGE_TOKENS: u64 = 1_000;

/// The tokens a run of text stands for: about four bytes per token.
/// No tokenizer runs this side of the wire, so every token figure the
/// Usage page shows is this estimate, and the page marks it as one.
pub fn text_tokens(bytes: usize) -> u64 {
    bytes as u64 / 4
}

/// The estimated tokens a message stands for: its text at about four
/// bytes per token, each image at [`IMAGE_TOKENS`]. Summed part by
/// part, so the split can price each category with the same rounding.
pub fn message_tokens(message: &Message) -> u64 {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::Text(text) => text_tokens(text.len()),
            Content::ToolCall(call) => text_tokens(call.name.len() + call.args.to_string().len()),
            Content::ToolResult(result) => {
                text_tokens(result.content.len()) + result.images.len() as u64 * IMAGE_TOKENS
            }
            Content::Image(_) => IMAGE_TOKENS,
            _ => text_tokens(unknown_bytes(content)),
        })
        .sum()
}

/// The images one message holds: tool-result images and bare image
/// parts. The count the view's image cap spends, and nothing else:
/// an image's bytes and tokens are the flat charges above.
pub fn message_images(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::ToolResult(result) => result.images.len(),
            Content::Image(_) => 1,
            _ => 0,
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

/// A user message as one digest line: every message, whole, on one
/// line, cut only past [`USER_BYTES`] and then with a marker carrying
/// the length it lost. The marker matters: text that disappears with
/// no sign reads as text that was never there.
fn user_line(text: &str) -> String {
    let flat = one_line(text);
    if flat.len() <= USER_BYTES {
        return flat;
    }
    format!("{} [{} B, cut]", head(&flat, USER_BYTES), flat.len())
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
    let mut images = 0usize;

    // Assistant heads stay a two-line preview: the range's shape is
    // what a future reader needs. The user's words come whole below.
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
                        user_heads.push(user_line(text));
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
                    images += result.images.len();
                }
                // A head quotes text; an image has none. It still
                // counts: a digest that skips the images it covers
                // hides what zoom is there to reopen.
                Content::Image(_) => images += 1,
                // A kind this build cannot read still occupies the
                // range: count it as one bare part, the same as an
                // image, so the digest does not report an empty range.
                _ => images += 1,
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
    if images > 0 {
        parts.push(format!(
            "{images} {}",
            if images == 1 { "image" } else { "images" }
        ));
    }
    parts.extend(assistant_heads.iter().map(|h| format!("assistant: {h}")));
    ((range.id, range.n), parts.join("; "))
}

/// The whole render of one message, as `zoom(id, 1)` returns it: user
/// and assistant text verbatim, tool calls and results spelled out.
/// Uncapped, the zoom pages.
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
                for image in &result.images {
                    out.push_str(&format!("\n[image: {}]", image.media_type.as_str()));
                }
            }
            Content::Image(image) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[image: {}]", image.media_type.as_str()));
            }
            // Zoom promises the messages whole: a kind this build
            // cannot render says so rather than reading as absent.
            _ => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("[content this build cannot render]");
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
    use rhema_core::{ToolCall, ToolResult};

    #[test]
    fn a_results_images_count_and_describe() {
        let message = Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(
                ToolResult::success("c1", "out").with_images(vec![rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "cG5n",
                )]),
            )],
        };
        // "out" plus one image at its flat budget charge, not the
        // base64 data's four bytes.
        assert_eq!(message_bytes(&message), 3 + IMAGE_BYTES);
        let rendered = render_message(&message);
        assert!(rendered.contains("[image: image/png]"), "{rendered}");
    }

    #[test]
    fn tokens_are_text_over_four_plus_a_flat_image_price() {
        let message = Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(
                ToolResult::success("c1", "abcdefgh").with_images(vec![
                    rhema_core::Image::new(rhema_core::ImageMediaType::Png, "a".repeat(4_000_000)),
                    rhema_core::Image::new(rhema_core::ImageMediaType::Png, "b".repeat(8)),
                ]),
            )],
        };
        // Eight bytes of text is two tokens; the two images are a
        // flat price each, whatever their base64 length.
        assert_eq!(message_tokens(&message), 2 + 2 * IMAGE_TOKENS);
    }

    /// The cap's unit: tool-result images and bare image parts count,
    /// text does not. The digest names the same figure, so a range's
    /// line says what zoom would bring back.
    #[test]
    fn the_image_count_is_its_own_figure() {
        let user = Message {
            annotations: Default::default(),
            role: Role::User,
            content: vec![
                Content::Text("look".into()),
                Content::Image(rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "cG5n",
                )),
            ],
        };
        let result = Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(
                ToolResult::success("c1", "out").with_images(vec![
                    rhema_core::Image::new(rhema_core::ImageMediaType::Png, "YQ"),
                    rhema_core::Image::new(rhema_core::ImageMediaType::Png, "Yg"),
                ]),
            )],
        };
        assert_eq!(message_images(&user), 1);
        assert_eq!(message_images(&result), 2);
        let messages = vec![user, result];
        let ((id, n), text) = summarize_range(&messages, Range { id: 0, n: 2 });
        assert_eq!((id, n), (0, 2));
        assert!(text.contains("3 images"), "{text}");
    }

    #[test]
    fn an_images_encoded_length_does_not_reach_the_budget() {
        let tiny = Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(
                ToolResult::success("c1", "").with_images(vec![rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "a".repeat(4_000_000),
                )]),
            )],
        };
        // Four megabytes of base64 is one message's worth of context.
        assert_eq!(message_bytes(&tiny), IMAGE_BYTES);
    }

    #[test]
    fn every_user_text_gets_a_line_whole() {
        let mut messages: Vec<Message> = (0..12)
            .map(|i| Message::text(Role::User, format!("decision {i}: keep this")))
            .collect();
        messages.push(Message::text(Role::Assistant, "done"));
        let (_, text) = summarize_range(&messages, Range { id: 0, n: 13 });
        for i in 0..12 {
            assert!(
                text.contains(&format!("user: decision {i}: keep this")),
                "{text}"
            );
        }
        assert!(!text.contains("cut"), "{text}");
    }

    #[test]
    fn a_user_text_past_the_cap_says_what_it_lost() {
        let long = "x".repeat(USER_BYTES + 500);
        let messages = vec![Message::text(Role::User, long)];
        let (_, text) = summarize_range(&messages, Range { id: 0, n: 1 });
        // The whole head, then the marker naming the bytes that did
        // not fit.
        let head = format!("user: {}", "x".repeat(USER_BYTES));
        assert!(
            text.starts_with(&head),
            "digest starts wrong: {}",
            &text[..64]
        );
        let marker = format!(" [{} B, cut]", USER_BYTES + 500);
        assert!(
            text.ends_with(&marker),
            "digest ends wrong: len {}",
            text.len()
        );
        assert_eq!(text.len(), head.len() + "\u{2026}".len() + marker.len());
    }

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
            annotations: Default::default(),
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
            annotations: Default::default(),
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
