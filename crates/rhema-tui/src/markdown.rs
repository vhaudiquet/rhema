//! Assistant markdown rendering: CommonMark in, styled ratatui lines out.
//!
//! The model speaks markdown; raw source is noise in a terminal. The whole
//! accumulated response is parsed at draw time with pulldown-cmark and
//! mapped onto styled lines — streaming stays append-only, and the wrap
//! pass downstream still counts rows exactly. Palette: code in electric
//! cyan, code blocks on slate, quotes and rules dim; no syntax highlighting
//! inside code blocks (syntect later if wanted).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

/// Electric cyan for code, per the design palette (#38bdf8).
fn code_color() -> Color {
    Color::Rgb(56, 189, 248)
}

/// Slate code-block background (#161b22).
fn code_block_bg() -> Color {
    Color::Rgb(22, 27, 34)
}

/// Renders one assistant response (its full markdown source) into styled
/// lines. Plain text passes through untouched.
pub fn render_markdown(source: &str) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::default();
    for event in Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH) {
        renderer.push(event);
    }
    renderer.finish()
}

/// The mapped styles for one inline run.
#[derive(Debug, Clone, Copy, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    link: bool,
}

impl InlineStyle {
    fn apply(self, text: String) -> Span<'static> {
        let mut style = Style::default();
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strikethrough {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link {
            style = style.fg(code_color()).underlined();
        }
        Span::styled(text, style)
    }
}

/// Accumulates parser events into lines.
#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    /// Spans of the line being built.
    spans: Vec<Span<'static>>,
    style: InlineStyle,
    /// Nesting depth of lists, for indentation.
    list_depth: usize,
    /// Counter of the running ordered list; `None` in unordered lists.
    ordered_counter: Option<u64>,
    /// Inside a fenced code block: rows carry the block background.
    in_code_block: bool,
}

impl MarkdownRenderer {
    fn push(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.separate();
                self.style.bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush_line();
                self.separate();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush_line();
                self.separate();
                self.in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.flush_line();
                self.in_code_block = false;
                self.separate();
            }
            Event::Start(Tag::List(start)) => {
                self.list_depth += 1;
                self.ordered_counter = start;
            }
            Event::End(TagEnd::List(_)) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                if self.list_depth == 0 {
                    self.ordered_counter = None;
                }
            }
            Event::Start(Tag::Item) => {
                self.flush_line();
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let marker = match &mut self.ordered_counter {
                    Some(counter) => {
                        let marker = format!("{counter}. ");
                        *counter += 1;
                        marker
                    }
                    None => "• ".to_owned(),
                };
                self.spans.push(Span::raw(format!("{indent}{marker}")));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_line();
                self.separate();
                self.spans.push(Span::raw("│ ").dim());
            }
            Event::Start(Tag::Strong) => self.style.bold = true,
            Event::End(TagEnd::Strong) => self.style.bold = false,
            Event::Start(Tag::Emphasis) => self.style.italic = true,
            Event::End(TagEnd::Emphasis) => self.style.italic = false,
            Event::Start(Tag::Strikethrough) => self.style.strikethrough = true,
            Event::End(TagEnd::Strikethrough) => self.style.strikethrough = false,
            Event::Start(Tag::Link { .. }) => self.style.link = true,
            Event::End(TagEnd::Link) => self.style.link = false,
            Event::Code(code) => self.spans.push(Span::styled(
                code.to_string(),
                Style::default().fg(code_color()),
            )),
            Event::Text(text) => {
                if self.in_code_block {
                    // One parser Text event per source line inside a fence.
                    for (i, row) in text.split('\n').enumerate() {
                        if i > 0 {
                            self.flush_line();
                        }
                        if !row.is_empty() {
                            self.spans.push(
                                Span::raw(row.to_owned())
                                    .fg(code_color())
                                    .bg(code_block_bg()),
                            );
                        }
                    }
                } else {
                    self.spans.push(self.style.apply(text.to_string()));
                }
            }
            Event::SoftBreak | Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.separate();
                self.lines.push(Line::from(Span::raw("────────").dim()));
                self.separate();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.spans
                    .push(Span::raw(html.to_string()).fg(Color::DarkGray));
            }
            _ => {}
        }
    }

    /// Closes the current line; an empty line is not pushed (block
    /// boundaries ask for blank rows explicitly via [`Self::separate`]).
    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.spans);
        if !spans.is_empty() {
            self.lines.push(Line::from(spans));
        }
        self.style = InlineStyle::default();
    }

    /// Asks for a blank row before the next content line, unless nothing
    /// was rendered yet or the previous line is already blank.
    fn separate(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // Trailing blank rows from block ends carry no information.
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn plain_text_passes_through_without_artifacts() {
        let lines = render_markdown("just words\nacross lines");
        assert_eq!(plain(&lines), ["just words", "across lines"]);
    }

    #[test]
    fn emphasis_and_code_render_styled_without_markers() {
        let lines = render_markdown("a **bold** and *soft* and `code` bit");
        assert_eq!(lines.len(), 1);
        let spans: Vec<(String, Style)> = lines[0]
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect();
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "bold" && style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            spans.iter().any(
                |(text, style)| text == "soft" && style.add_modifier.contains(Modifier::ITALIC)
            )
        );
        let code = spans.iter().find(|(text, _)| text == "code").unwrap();
        assert_eq!(code.1.fg, Some(code_color()));
    }

    #[test]
    fn headings_render_bold_and_separated() {
        let lines = render_markdown("# Title\n\nbody");
        let rendered = plain(&lines);
        assert_eq!(rendered[0], "Title");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(rendered.iter().filter(|l| l.is_empty()).count(), 1);
    }

    #[test]
    fn code_blocks_keep_shape_and_gain_the_background() {
        let source = "```rust\nfn main() {\n    done();\n}\n```";
        let lines = render_markdown(source);
        let rendered = plain(&lines);
        assert_eq!(rendered[0], "fn main() {");
        assert_eq!(rendered[1], "    done();");
        assert_eq!(
            lines[0].spans[0].style.bg,
            Some(code_block_bg()),
            "code rows carry the slate background"
        );
    }

    #[test]
    fn lists_render_bullets_with_indent() {
        let source = "- one\n- two\n  - nested";
        let lines = render_markdown(source);
        let rendered = plain(&lines);
        assert_eq!(rendered[0], "• one");
        assert_eq!(rendered[1], "• two");
        assert_eq!(rendered[2], "  • nested");
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let lines = render_markdown("1. first\n2. second");
        let rendered = plain(&lines);
        assert_eq!(rendered[0], "1. first");
        assert_eq!(rendered[1], "2. second");
    }
}
