//! The interactive approval hook: shows the gated call on stderr and asks
//! on stdin.
//!
//! STDIN SHARING CONSTRAINT: the REPL prompt and this hook read the same
//! stdin, but they can never overlap. The REPL blocks inside
//! `CodingAgent::turn` while a turn runs, and tool resolution is
//! synchronous within a turn (SPEC.md §4) — so the approval prompt fires
//! while the REPL is parked in the turn call, and the next REPL prompt is
//! printed only after the turn (and every prompt) has finished reading.

use std::io::{BufRead, Write};

use rhema_coding_agent::Approval;
use rhema_core::ToolCall;

/// Pretty-printed lines and characters before the args are cut.
const ARGS_MAX_LINES: usize = 12;
const ARGS_MAX_CHARS: usize = 400;

/// Asks the user on the terminal for every gated call. Anything but
/// `y`/`yes` (case-insensitive) declines; EOF and read errors decline, so a
/// detached run fails safe instead of auto-approving.
pub struct ConsoleApproval;

impl Approval for ConsoleApproval {
    fn approve(&self, call: &ToolCall) -> bool {
        eprintln!(
            "approval requested: {} {}",
            call.name,
            pretty_truncated(&call.args)
        );
        eprint!("allow? [y/N] ");
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => false,
            Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        }
    }
}

/// Args as pretty JSON, cut to a terminal-friendly size.
pub fn pretty_truncated(args: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    truncate_lines(&pretty, ARGS_MAX_LINES, ARGS_MAX_CHARS)
}

/// Cuts `text` to the first `max_lines` lines and `max_chars` characters
/// (on a char boundary), appending a marker when anything was cut.
fn truncate_lines(text: &str, max_lines: usize, max_chars: usize) -> String {
    let mut kept: Vec<&str> = text.lines().take(max_lines).collect();
    let mut joined = kept.join("\n");
    if joined.chars().count() > max_chars {
        let mut end = max_chars;
        while !joined.is_char_boundary(end) {
            end -= 1;
        }
        joined.truncate(end);
        kept = joined.lines().collect();
        joined = kept.join("\n");
    }
    if text.lines().count() > max_lines || joined.chars().count() < text.chars().count() {
        joined.push_str("\n\u{2026}[truncated]");
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn small_args_print_in_full() {
        let args = json!({ "path": "notes.txt" });
        assert_eq!(pretty_truncated(&args), "{\n  \"path\": \"notes.txt\"\n}");
    }

    #[test]
    fn wide_args_are_cut_on_a_char_boundary() {
        let args = json!({ "content": "héllo".repeat(300) });
        let out = pretty_truncated(&args);
        assert!(out.chars().count() < ARGS_MAX_CHARS + 20);
        assert!(out.ends_with("[truncated]"));
        // No panic and no split multi-byte char: it round-trips as text.
        assert!(out.contains("h"));
    }

    #[test]
    fn tall_args_are_cut_by_line_count() {
        let content = serde_json::Value::Array((0..100).map(|i| json!(i)).collect());
        let args = json!({ "content": content });
        let out = pretty_truncated(&args);
        assert_eq!(out.lines().count(), ARGS_MAX_LINES + 1); // + the marker
        assert!(out.ends_with("[truncated]"));
    }
}
