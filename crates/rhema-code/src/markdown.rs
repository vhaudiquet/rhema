//! Assistant markdown rendering: CommonMark in, styled ratatui lines out.
//!
//! The model speaks markdown; raw source is noise in a terminal. The whole
//! accumulated response is parsed at draw time with pulldown-cmark and
//! mapped onto styled lines, streaming stays append-only, and the wrap
//! pass downstream still counts rows exactly. Palette: code in electric
//! cyan, code blocks on slate, quotes and rules dim; no syntax highlighting
//! inside code blocks (syntect later if wanted).

use crate::palette::Theme;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One run of cells on a rendered row that stands for a URL: a markdown
/// link's label as it landed on the row. A click inside the run opens the
/// URL, so the transcript needs the cells and not just the label text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// Index into the lines rendered beside it.
    pub row: usize,
    /// The run's first display column, counted from the row's start.
    pub start: usize,
    /// One past the run's last display column.
    pub end: usize,
    /// The link's target.
    pub url: String,
}

/// Renders one assistant response (its full markdown source) into styled
/// lines at `width` columns, tables lay their columns out against it.
/// Plain text passes through untouched. Code reads in the accent's text
/// tone on a surface well, the transcript well is sunken, so code steps
/// one tier up.
pub fn render_markdown(source: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_links(source, theme, width).0
}

/// [`render_markdown`], plus the cells every link's label covers. The
/// rows and columns describe the returned lines; the transcript shifts
/// them onto the screen when it lays the frame out.
pub fn render_markdown_with_links(
    source: &str,
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<LinkSpan>) {
    let mut renderer = MarkdownRenderer::new(theme, width);
    for event in Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    ) {
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
    fn apply(self, text: String, theme: &Theme) -> Span<'static> {
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
            style = style.fg(theme.accent_text).underlined();
        }
        Span::styled(text, style)
    }
}

/// Accumulates parser events into lines.
struct MarkdownRenderer<'a> {
    lines: Vec<Line<'static>>,
    /// Spans of the line being built.
    spans: Vec<Span<'static>>,
    style: InlineStyle,
    /// Per-level list state: the running number of ordered lists, `None`
    /// for unordered ones. A stack, because a nested list must not
    /// clobber its parent's counter; its length is the nesting depth.
    counters: Vec<Option<u64>>,
    /// The content column of each open item, its marker's own width
    /// included. A paragraph that follows a blank row inside an item
    /// starts here, so it stays visually inside the bullet instead of
    /// drifting back to the margin.
    item_indents: Vec<usize>,
    /// Inside a fenced code block: rows carry the block background.
    in_code_block: bool,
    /// Inside a block quote: every line carries the rail.
    in_block_quote: bool,
    /// Inside a markdown table: cells collect as plain text, the whole
    /// table renders aligned at its end.
    in_table: bool,
    table_header: Option<Vec<String>>,
    table_rows: Vec<Vec<String>>,
    table_row: Vec<String>,
    table_cell: Option<String>,
    /// The link's target while inside its label; `None` outside.
    link_url: Option<String>,
    /// The link labels collected so far, in rendered coordinates.
    links: Vec<LinkSpan>,
    /// The design tokens; colors never come from literals (rule 1).
    theme: &'a Theme,
    /// The columns tables lay out against, the transcript's width.
    width: usize,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(theme: &'a Theme, width: usize) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style: InlineStyle::default(),
            counters: Vec::new(),
            item_indents: Vec::new(),
            in_code_block: false,
            in_block_quote: false,
            in_table: false,
            table_header: None,
            table_rows: Vec::new(),
            table_row: Vec::new(),
            table_cell: None,
            link_url: None,
            links: Vec::new(),
            theme,
            width,
        }
    }

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
            Event::End(TagEnd::Paragraph) => {
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
                self.flush_line();
                // A top-level list steps away from the text before it; a
                // sublist hugs its parent item, the blank row is what
                // made nesting read as same-level items. The paragraph
                // that introduces the sublist already asked for a blank
                // row at its own end, so the hug means taking it back.
                if self.counters.is_empty() {
                    self.separate();
                } else if self.lines.last().is_some_and(|line| line.spans.is_empty()) {
                    self.lines.pop();
                }
                self.counters.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.flush_line();
                self.counters.pop();
                if self.counters.is_empty() {
                    self.separate();
                }
            }
            Event::Start(Tag::Item) => {
                self.flush_line();
                let depth = self.counters.len();
                let indent = "  ".repeat(depth.saturating_sub(1));
                let marker = match self.counters.last_mut() {
                    Some(Some(counter)) => {
                        let marker = format!("{counter}. ");
                        *counter += 1;
                        marker
                    }
                    // Unordered markers differ per depth, so the eye
                    // pairs every subitem with its parent.
                    Some(None) => match depth {
                        1 => "• ",
                        2 => "◦ ",
                        _ => "▪ ",
                    }
                    .to_owned(),
                    None => "• ".to_owned(),
                };
                self.item_indents.push(indent.width() + marker.width());
                self.spans.push(Span::raw(format!("{indent}{marker}")));
            }
            Event::End(TagEnd::Item) => {
                self.flush_line();
                self.item_indents.pop();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_line();
                self.separate();
                self.in_block_quote = true;
                self.spans.push(Span::raw("│ ").fg(self.theme.text3));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_line();
                self.in_block_quote = false;
                self.separate();
            }
            Event::Start(Tag::Table(_)) => {
                self.flush_line();
                self.separate();
                self.in_table = true;
                self.table_rows.clear();
                self.table_header = None;
            }
            Event::Start(Tag::TableHead) => self.table_row.clear(),
            Event::End(TagEnd::TableHead) => {
                self.table_header = Some(std::mem::take(&mut self.table_row));
            }
            Event::Start(Tag::TableRow) => self.table_row.clear(),
            Event::End(TagEnd::TableRow) => {
                let row = std::mem::take(&mut self.table_row);
                if !row.is_empty() {
                    self.table_rows.push(row);
                }
            }
            Event::Start(Tag::TableCell) => self.table_cell = Some(String::new()),
            Event::End(TagEnd::TableCell) => {
                if let Some(cell) = self.table_cell.take() {
                    self.table_row.push(cell);
                }
            }
            Event::End(TagEnd::Table) => {
                self.in_table = false;
                let rows = std::mem::take(&mut self.table_rows);
                let header = self.table_header.take().unwrap_or_default();
                for line in render_table(&header, &rows, self.width) {
                    self.lines.push(line);
                }
                self.separate();
            }
            Event::Start(Tag::Strong) => self.style.bold = true,
            Event::End(TagEnd::Strong) => self.style.bold = false,
            Event::Start(Tag::Emphasis) => self.style.italic = true,
            Event::End(TagEnd::Emphasis) => self.style.italic = false,
            Event::Start(Tag::Strikethrough) => self.style.strikethrough = true,
            Event::End(TagEnd::Strikethrough) => self.style.strikethrough = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.style.link = true;
                self.link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                self.style.link = false;
                self.link_url = None;
            }
            Event::Code(code) => match &mut self.table_cell {
                Some(cell) => cell.push_str(&code),
                None => {
                    self.note_link(&code);
                    self.spans.push(Span::styled(
                        code.to_string(),
                        Style::default().fg(self.theme.accent_text),
                    ));
                }
            },
            Event::Text(text) => {
                if let Some(cell) = &mut self.table_cell {
                    cell.push_str(&text);
                } else if self.in_code_block {
                    // One parser Text event per source line inside a fence.
                    // Every row, blank ones too, carries the slate to the
                    // full width: a block reads as one solid slab, not as
                    // patches that stop where each line's text stops.
                    for (i, row) in text.split('\n').enumerate() {
                        if i > 0 {
                            self.flush_line();
                        }
                        if !row.is_empty() {
                            self.spans.push(
                                Span::raw(row.to_owned())
                                    .fg(self.theme.accent_text)
                                    .bg(self.theme.surface),
                            );
                        }
                        let pad = self.width.saturating_sub(row.width());
                        if pad > 0 {
                            self.spans.push(
                                Span::raw(" ".repeat(pad))
                                    .fg(self.theme.accent_text)
                                    .bg(self.theme.surface),
                            );
                        }
                    }
                } else {
                    self.indent_line();
                    self.note_link(&text);
                    self.spans
                        .push(self.style.apply(text.to_string(), self.theme));
                }
            }
            // A soft break joins the paragraph with a space, models
            // hard-wrap prose mid-sentence (some streams even pad chunks
            // with newlines), and slicing every source line into its own
            // row shreds such output. An explicit hard break still cuts
            // the row, and quoted lines keep their rail.
            Event::SoftBreak => {
                if let Some(cell) = &mut self.table_cell {
                    cell.push(' ');
                } else if self.in_block_quote {
                    self.flush_line();
                    self.spans.push(Span::raw("│ ").fg(self.theme.text3));
                } else if !self.spans.is_empty() {
                    self.spans.push(Span::raw(" "));
                }
            }
            Event::HardBreak => {
                if let Some(cell) = &mut self.table_cell {
                    cell.push(' ');
                } else {
                    self.flush_line();
                    if self.in_block_quote {
                        self.spans.push(Span::raw("│ ").fg(self.theme.text3));
                    }
                }
            }
            Event::Rule => {
                self.flush_line();
                self.separate();
                self.lines
                    .push(Line::from(Span::raw("────────").fg(self.theme.text3)));
                self.separate();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.spans
                    .push(Span::raw(html.to_string()).fg(Color::DarkGray));
            }
            _ => {}
        }
    }

    /// Records a link label's cells before the span is pushed. The
    /// columns count this row's spans so far, the cells the frame will
    /// draw; a label split by a soft break records one run per piece.
    fn note_link(&mut self, text: &str) {
        let Some(url) = &self.link_url else {
            return;
        };
        if text.is_empty() || !opens_externally(url) {
            return;
        }
        let start: usize = self.spans.iter().map(|span| span.content.width()).sum();
        let end = start + text.width();
        self.links.push(LinkSpan {
            row: self.lines.len(),
            start,
            end,
            url: url.clone(),
        });
    }

    /// Pads the line being started to the innermost open item's content
    /// column. A line the renderer already began, an item's marker, a
    /// quote's rail, a soft break's join, keeps its own shape; only a
    /// fresh row inside an item is padded.
    fn indent_line(&mut self) {
        if self.spans.is_empty()
            && let Some(columns) = self.item_indents.last().copied().filter(|cols| *cols > 0)
        {
            self.spans.push(Span::raw(" ".repeat(columns)));
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

    fn finish(mut self) -> (Vec<Line<'static>>, Vec<LinkSpan>) {
        self.flush_line();
        // Trailing blank rows from block ends carry no information. A
        // link that sat on one of them goes with it.
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        let rows = self.lines.len();
        self.links.retain(|link| link.row < rows);
        (self.lines, self.links)
    }
}

/// Whether a markdown target is a URL a click may hand to the desktop
/// browser. Only the web schemes pass: a link the model wrote must not
/// launch a local file or an arbitrary URI handler.
fn opens_externally(url: &str) -> bool {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme).unwrap_or("");
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// A table as aligned plain columns: a bold header, a rule under it,
/// cells left-aligned and cut to their column.
/// Space between table columns.
const TABLE_COLUMN_GAP: usize = 2;

/// Greedy word wrap of one cell into its column, hard-splitting words
/// longer than the column. Cells are plain text, so this works on
/// strings instead of styled rows.
fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in cell.split_whitespace() {
        let mut word = word;
        loop {
            if word.width() > width {
                // A word longer than the whole column hard-splits, at a
                // display-width boundary: a wide character takes more
                // than one cell, and a zero-width lead (a stray
                // combining mark) must not stall the cut, so the first
                // character always goes.
                let mut cut = 0;
                let mut used = 0;
                for (index, ch) in word.char_indices() {
                    let cells = ch.width().unwrap_or(0);
                    if used + cells > width && cut > 0 {
                        break;
                    }
                    used += cells;
                    cut = index + ch.len_utf8();
                }
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                rows.push(word[..cut].to_owned());
                word = &word[cut..];
                continue;
            }
            if row.is_empty() {
                // The hard-split above guarantees a bare word fits.
                row.push_str(word);
            } else if row.width() + 1 + word.width() <= width {
                row.push(' ');
                row.push_str(word);
            } else {
                rows.push(std::mem::take(&mut row));
                continue;
            }
            break;
        }
    }
    rows.push(row);
    rows
}

/// A table laid out at the terminal's width: columns fit their content
/// when there is room, share what remains when there is not, and cell
/// content wraps onto continuation rows, model output is never cut.
fn render_table(header: &[String], rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let cols = header
        .len()
        .max(rows.iter().map(|row| row.len()).max().unwrap_or(0));
    if cols == 0 {
        return Vec::new();
    }
    fn cell_at(row: &[String], col: usize) -> &str {
        row.get(col).map(String::as_str).unwrap_or("")
    }
    let natural: Vec<usize> = (0..cols)
        .map(|col| {
            std::iter::once(header)
                .chain(rows.iter().map(Vec::as_slice))
                .map(|row| cell_at(row, col).width())
                .max()
                .unwrap_or(0)
        })
        .collect();
    // Fit-then-share: a column takes its natural width while that stays
    // within an even share of the space; unsatisfied columns split what
    // remains, and wrap their cells.
    let available = width
        .saturating_sub(TABLE_COLUMN_GAP * (cols - 1))
        .max(cols);
    let mut widths = vec![0usize; cols];
    let mut budget = available;
    let mut pending: Vec<usize> = (0..cols).collect();
    while !pending.is_empty() {
        let share = budget / pending.len();
        let satisfied: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|col| natural[*col] <= share)
            .collect();
        if satisfied.is_empty() {
            for col in &pending {
                widths[*col] = share.max(1);
            }
            break;
        }
        for col in satisfied {
            widths[col] = natural[col];
            budget -= natural[col];
            pending.retain(|pending_col| *pending_col != col);
        }
    }

    fn emit(row: &[String], style: Style, widths: &[usize], lines: &mut Vec<Line<'static>>) {
        let cells: Vec<Vec<String>> = (0..widths.len())
            .map(|col| wrap_cell(cell_at(row, col), widths[col]))
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans = Vec::new();
            for (col, cell) in cells.iter().enumerate() {
                let text = cell.get(line_index).cloned().unwrap_or_default();
                if col + 1 == widths.len() {
                    spans.push(Span::styled(text, style));
                } else {
                    // Pad by display width: format!'s `<` counts
                    // characters, and a wide cell would under-pad and
                    // leave the columns ragged.
                    let pad = widths[col].saturating_sub(text.width());
                    spans.push(Span::styled(format!("{text}{}", " ".repeat(pad)), style));
                    spans.push(Span::raw(" ".repeat(TABLE_COLUMN_GAP)));
                }
            }
            lines.push(Line::from(spans));
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    emit(
        header,
        Style::default().add_modifier(Modifier::BOLD),
        &widths,
        &mut lines,
    );
    lines.push(Line::from(Span::raw(
        widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>()
            .join("  "),
    )));
    for row in rows {
        emit(row, Style::default(), &widths, &mut lines);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::DARK;

    fn render_markdown_d(source: &str) -> Vec<Line<'static>> {
        render_markdown(source, &crate::palette::DARK, 80)
    }

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
        let lines = render_markdown_d("just words across lines");
        assert_eq!(plain(&lines), ["just words across lines"]);
    }

    #[test]
    fn soft_wrapped_prose_flows_instead_of_slicing() {
        // The fragmented shape some model streams arrive in.
        let lines = render_markdown_d(
            "rebase\n master only\n .\n the rebuild\n (with backup ref\n s everywhere):",
        );
        assert_eq!(
            plain(&lines).len(),
            1,
            "one paragraph, one row: {:#?}",
            plain(&lines)
        );
        let joined: String = plain(&lines)[0]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("backup ref s everywhere"), "{joined}");
    }

    #[test]
    fn a_hard_break_still_cuts_the_row() {
        let lines = render_markdown_d("first  \nsecond");
        assert_eq!(plain(&lines), ["first", "second"]);
    }

    #[test]
    fn emphasis_and_code_render_styled_without_markers() {
        let lines = render_markdown_d("a **bold** and *soft* and `code` bit");
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
        assert_eq!(code.1.fg, Some(DARK.accent_text));
    }

    /// A link's label reports the cells it covers and the URL beside
    /// them, so a click can open what the label stands for.
    #[test]
    fn link_labels_report_their_cells_and_target() {
        let (lines, links) = render_markdown_with_links(
            "read [the docs](https://example.com/x) now",
            &crate::palette::DARK,
            80,
        );
        assert_eq!(plain(&lines), ["read the docs now"]);
        assert_eq!(links.len(), 1, "{links:?}");
        let link = &links[0];
        assert_eq!(link.row, 0);
        assert_eq!(link.start, 5, "the label starts after `read `");
        assert_eq!(link.url, "https://example.com/x");
        let covered: String = plain(&lines)[0]
            .chars()
            .skip(link.start)
            .take(link.end - link.start)
            .collect();
        assert_eq!(covered, "the docs");
    }

    /// A click may only hand the browser a web URL: a model-written
    /// `file:` or custom scheme opens nothing and stays plain text.
    #[test]
    fn only_web_links_are_clickable() {
        let (_, links) = render_markdown_with_links(
            "[local](file:///etc/passwd) and [web](https://example.com)",
            &crate::palette::DARK,
            80,
        );
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "https://example.com");
    }

    /// A link's cells count the row's other spans: the quote rail and a
    /// list marker sit before the label, and a click lands on the label
    /// only when the columns include them.
    #[test]
    fn link_cells_include_the_marker_before_the_label() {
        let (lines, links) = render_markdown_with_links(
            "> quote [docs](https://example.com)",
            &crate::palette::DARK,
            80,
        );
        let line = plain(&lines)[0].clone();
        let link = &links[0];
        let covered: String = line
            .chars()
            .skip(link.start)
            .take(link.end - link.start)
            .collect();
        assert_eq!(covered, "docs");
        assert_eq!(link.start, "│ quote ".width());
    }

    #[test]
    fn block_quote_lines_all_carry_the_rail_and_separate() {
        let lines = render_markdown_d("before\n\n> quoted one\n> quoted two\n\nafter");
        let rendered = plain(&lines);
        assert_eq!(
            rendered,
            ["before", "", "│ quoted one", "│ quoted two", "", "after"]
        );
    }

    #[test]
    fn consecutive_paragraphs_get_a_blank_row_between_them() {
        let lines = render_markdown_d("first paragraph\n\nsecond paragraph");
        let rendered = plain(&lines);
        assert_eq!(rendered, ["first paragraph", "", "second paragraph"]);
    }

    #[test]
    fn lists_are_separated_from_the_surrounding_text() {
        let lines = render_markdown_d("intro:\n\n- one\n- two\n\noutro");
        let rendered = plain(&lines);
        assert_eq!(rendered, ["intro:", "", "• one", "• two", "", "outro"]);
    }

    #[test]
    fn headings_render_bold_and_separated() {
        let lines = render_markdown_d("# Title\n\nbody");
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
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(rendered[0].trim_end(), "fn main() {");
        assert_eq!(rendered[1].trim_end(), "    done();");
        assert_eq!(
            lines[0].spans[0].style.bg,
            Some(DARK.surface),
            "code rows carry the slate background"
        );
    }

    #[test]
    fn code_rows_carry_the_slate_to_the_full_width() {
        // The slab must be solid: a block whose rows stop where their text
        // stops reads as staircase patches, not as a block.
        let source = "```text\nshort\n\n\nwide line of code\n```";
        let lines = render_markdown_d(source);
        // Four content rows plus the fence's trailing newline: the
        // block's bottom pad band.
        assert_eq!(lines.len(), 5, "blank code lines render as rows");
        for line in &lines {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            assert_eq!(width, 80, "one solid row: {:?}", plain(&lines));
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.style.bg == Some(DARK.surface)),
                "the padding carries the slate too"
            );
        }
    }

    #[test]
    fn sublists_hug_their_parent_and_take_a_deeper_marker() {
        let source = "- one\n- two\n  - nested";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(rendered, ["• one", "• two", "  ◦ nested"]);
    }

    #[test]
    fn the_ordered_counter_survives_a_nested_list() {
        let source = "- item\n  1. a\n  2. b\n- item2";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(rendered, ["• item", "  1. a", "  2. b", "• item2"]);
    }

    #[test]
    fn ordered_items_keep_numbering_across_a_nested_unordered_list() {
        let source = "1. one\n   - sub\n2. two";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(rendered, ["1. one", "  ◦ sub", "2. two"]);
    }

    /// A paragraph after a blank row inside an item belongs to the
    /// item: it starts at the item's content column, marker width
    /// included, instead of drifting back to the margin. The subagent
    /// tool's description relies on this for its catalog lines.
    #[test]
    fn a_paragraph_inside_an_item_keeps_the_item_indent() {
        let source = "- subagent | the schema\n  Run it.\n\n  Subagents, by name:\n  - one: does things.\n\n  Where it runs: here.";
        assert_eq!(
            plain(&render_markdown_d(source)),
            [
                "• subagent | the schema Run it.",
                "",
                "  Subagents, by name:",
                "  ◦ one: does things.",
                "  Where it runs: here.",
            ]
        );
    }

    /// The paragraph that introduces a sublist and the sublist itself
    /// stay one block: the paragraph's trailing blank row is taken back
    /// when the list belongs to the same item.
    #[test]
    fn a_paragraph_introducing_a_sublist_hugs_it() {
        let source = "- item\n\n  lead-in:\n  - sub";
        assert_eq!(
            plain(&render_markdown_d(source)),
            ["• item", "", "  lead-in:", "  ◦ sub"]
        );
    }

    /// The indent is the marker's own width: an ordered item's
    /// continuation lines align under the text after `1. `, not under
    /// the bullet a two-space rule would assume.
    #[test]
    fn the_indent_tracks_the_markers_width() {
        let source = "1. item\n\n   continued";
        assert_eq!(
            plain(&render_markdown_d(source)),
            ["1. item", "", "   continued"]
        );
    }

    #[test]
    fn text_after_a_list_returns_to_the_margin() {
        let source = "- one\n\n  in the item\n\nafter";
        assert_eq!(
            plain(&render_markdown_d(source)),
            ["• one", "", "  in the item", "", "after"]
        );
    }

    #[test]
    fn tables_render_as_aligned_columns_with_a_header_rule() {
        let source = "intro:\n\n| a | bb |\n|---|---|\n| 1 | 2 |\n| 33 | 4 |\n\noutro";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(
            rendered,
            [
                "intro:",
                "",
                "a   bb",
                "──  ──",
                "1   2",
                "33  4",
                "",
                "outro",
            ]
        );
    }

    #[test]
    fn table_headers_are_bold_and_wide_content_wraps_never_cuts() {
        let long = "x".repeat(40);
        let source = format!("| head | h2 |\n|---|---|\n| {long} | v |");
        let lines = render_markdown_d(&source);
        // The header leads, bold.
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let all: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        // Nothing is lost: every x renders, and no ellipsis ever appears.
        assert_eq!(all.matches('x').count(), 40, "{all}");
        assert!(!all.contains('…'), "model output is never cut: {all}");
    }

    #[test]
    fn table_columns_pad_by_display_width_for_wide_text() {
        // CJK characters occupy two cells each: padding by character
        // count would under-fill the column and leave the next one
        // ragged, and the rule row would disagree with the data.
        let source = "| \u{6f22} | x |\n|---|---|\n| \u{5b57} | y |";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(
            rendered,
            ["\u{6f22}  x", "\u{2500}\u{2500}  \u{2500}", "\u{5b57}  y"]
        );
    }

    #[test]
    fn table_cells_capture_inline_markup_as_plain_text() {
        let source = "| c |\n|---|\n| `code` and **bold** |";
        let lines = render_markdown_d(source);
        let rendered = plain(&lines);
        assert_eq!(rendered[2], "code and bold");
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let lines = render_markdown_d("1. first\n2. second");
        let rendered = plain(&lines);
        assert_eq!(rendered[0], "1. first");
        assert_eq!(rendered[1], "2. second");
    }
}
