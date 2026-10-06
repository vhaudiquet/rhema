//! The terminal sink: renders the engine's events as they stream.
//!
//! Text deltas go to stdout (flushed per delta so streaming reads live);
//! everything else goes to stderr, keeping stdout clean for the model's
//! text. Tool-call arguments are accumulated per call for parity with the
//! stream, though the current rendering shows only the result summary.

use std::io::Write;

use rhema_core::{Event, EventSink, Usage};

/// Width at which a tool-result summary line is cut.
const SUMMARY_WIDTH: usize = 80;

/// Renders [`Event`]s to the terminal: text to stdout, lifecycle and tool
/// traffic to stderr. One instance per process; call [`TermSink::begin_turn`]
/// before each turn so step counts stay per-turn.
pub struct TermSink {
    /// Arguments of the tool call currently streaming.
    args: String,
    /// `StepStart` events seen since the turn began.
    steps: u32,
    /// Whether any text streamed this turn; used to keep the usage line off
    /// the tail of streamed output.
    streamed_text: bool,
}

impl Default for TermSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TermSink {
    /// A sink with no accumulated state.
    pub fn new() -> Self {
        Self {
            args: String::new(),
            steps: 0,
            streamed_text: false,
        }
    }

    /// Resets per-turn bookkeeping (step count, streamed-text flag, any
    /// unterminated argument buffer).
    pub fn begin_turn(&mut self) {
        self.args.clear();
        self.steps = 0;
        self.streamed_text = false;
    }

    /// Steps started within the current turn; drives the usage line.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Whether any text streamed since [`TermSink::begin_turn`].
    pub fn text_streamed(&self) -> bool {
        self.streamed_text
    }
}

impl EventSink for TermSink {
    fn emit(&mut self, event: Event<'_>) {
        match event {
            Event::StepStart { .. } => self.steps += 1,
            Event::TextDelta(delta) => {
                self.streamed_text = true;
                // Ignore I/O failures (e.g. a closed stdout) rather than
                // aborting the stream mid-turn.
                let _ = write_and_flush(&mut std::io::stdout().lock(), delta.as_bytes());
            }
            Event::ToolCallStart { id, name } => {
                let _ = writeln_and_flush(
                    &mut std::io::stderr().lock(),
                    &format!("\n→ tool {name} ({id})"),
                );
            }
            Event::ToolCallArgsDelta(delta) => self.args.push_str(delta),
            Event::ToolResult {
                content, is_error, ..
            } => {
                let mark = if is_error { "✗" } else { "✓" };
                let _ = writeln_and_flush(
                    &mut std::io::stderr().lock(),
                    &format!("  {mark} {}", summarize(content)),
                );
                self.args.clear();
            }
            // Usage is printed once per turn by the driver, from the final
            // `ModelOutput`, not per step.
            Event::StepEnd { .. } => {}
        }
    }
}

fn write_and_flush(mut out: impl Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(bytes)?;
    out.flush()
}

fn writeln_and_flush(mut out: impl Write, line: &str) -> std::io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// First line of a tool result, cut to `SUMMARY_WIDTH` columns.
pub fn summarize(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or("");
    truncate_chars(first_line, SUMMARY_WIDTH)
}

/// Cuts `text` to at most `max` characters, appending `…` when cut.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut end = max - 1;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &text[..end])
}

/// The one usage line printed after a turn: step count and the final
/// `ModelOutput`'s token accounting.
pub fn format_usage(steps: u32, usage: Usage) -> String {
    format!(
        "· {} steps, tokens: {} in / {} out ({} cached)",
        steps, usage.input_tokens, usage.output_tokens, usage.cached_input_tokens
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_takes_the_first_line() {
        assert_eq!(summarize("exit code 0\n--- stdout ---\nhi"), "exit code 0");
        assert_eq!(summarize(""), "");
    }

    #[test]
    fn summarize_cuts_long_lines_with_an_ellipsis() {
        let long = "x".repeat(200);
        let cut = summarize(&long);
        assert_eq!(cut.chars().count(), SUMMARY_WIDTH);
        assert!(cut.ends_with('…'));

        let multiline = format!("{long}\nsecond line");
        assert_eq!(summarize(&multiline), cut);
    }

    #[test]
    fn truncation_stays_on_a_char_boundary() {
        let cut = truncate_chars("héllo wörld", 4);
        assert_eq!(cut, "hé…");
    }

    #[test]
    fn usage_line_matches_the_specified_shape() {
        let usage = Usage {
            input_tokens: 1234,
            output_tokens: 567,
            cached_input_tokens: 891,
        };
        assert_eq!(
            format_usage(12, usage),
            "· 12 steps, tokens: 1234 in / 567 out (891 cached)"
        );
    }

    #[test]
    fn the_sink_counts_steps_and_resets_per_turn() {
        let mut sink = TermSink::new();
        sink.emit(Event::StepStart { iteration: 0 });
        sink.emit(Event::TextDelta("hi"));
        sink.emit(Event::StepStart { iteration: 1 });
        assert_eq!(sink.steps(), 2);
        assert!(sink.text_streamed());

        sink.begin_turn();
        assert_eq!(sink.steps(), 0);
        assert!(!sink.text_streamed());
    }
}
