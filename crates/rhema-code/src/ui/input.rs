//! The input box: segment wrapping, paste and image chips, and the cursor
//! mapping onto the wrapped rows.

use super::*;

pub(crate) fn render_input(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // The input is the one interactive element, so it earns the accent —
    // while a turn runs too, because the box stays live: typing steers
    // the running turn, and the border must not suggest otherwise.
    let border = theme.accent;
    // No fill of its own: the input sits on the same canvas as everything
    // else; only the border marks it.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The full inner width: `input_layout` subtracts the marker gutter
    // itself, and `input_rows` sizes the box from the same border-minus-
    // gutter width. Wrapping narrower here than there would clip the last
    // row under the box until the next keystroke grew it.
    let width = inner.width as usize;
    let prompt = "❯ ";
    // The layout is the render's source of truth: text rows with the
    // anchors inline, as chips, exactly as they sit in the input.
    let layout = input_layout(&app.chat.input, &app.chat.chips, width, theme);
    let cursor_at = cursor_at_rows(&layout, app.chat.cursor);
    // The window keeps the cursor's row visible.
    let remaining = INPUT_MAX_CONTENT_ROWS.max(1);
    let total = layout.len();
    let (cursor_row, cursor_col) = cursor_at.unwrap_or((total.saturating_sub(1), 0));
    let first = if total > remaining {
        cursor_row.min(total - remaining)
    } else {
        0
    };
    let rows: Vec<Line> = if layout.is_empty() {
        // The placeholder keeps the box reading as an input.
        vec![Line::from(vec![
            Span::raw(prompt).fg(theme.text3),
            Span::raw("ask anything · /help for commands").fg(theme.text3),
        ])]
    } else {
        layout[first..first + remaining.min(total - first)]
            .iter()
            .map(|row| row.line.clone())
            .collect()
    };
    frame.render_widget(Paragraph::new(rows), inner);
    // The cursor stays during a turn: the box takes input the whole
    // time. The prompt and the continuation indent are both two cells,
    // and only rows carrying one shift the cursor by them. A cursor
    // past a full row's last cell clamps onto it — there is no
    // column beyond the box.
    let marker = u16::from(layout[cursor_row].marker);
    frame.set_cursor_position(Position {
        x: (inner.x + marker * 2 + cursor_col as u16).min(inner.right().saturating_sub(1)),
        y: inner.y + cursor_row.saturating_sub(first) as u16,
    });
}

/// Content rows the input box may grow to; past that, a window keeps the
/// cursor's line shown.
const INPUT_MAX_CONTENT_ROWS: usize = 8;

/// One visual row of the input box: the rendered line plus the
/// bookkeeping the cursor mapping and the height math read.
#[derive(Debug, Clone)]
pub(crate) struct InputRow {
    pub(crate) line: Line<'static>,
    /// Source characters of `input` this row accounts for — a chip
    /// accounts for its single anchor character.
    pub(crate) source_chars: usize,
    /// Placed chips as `(source offset, display column, display width)`
    /// within the row's text: a chip is one source step but spans
    /// several cells, and the cursor mapping crosses it in one step.
    pub(crate) chips: Vec<(usize, usize, usize)>,
    /// Whether the row carries the two-cell logical-line marker.
    marker: bool,
    /// Whether the row ends its logical line — the newline character is
    /// one cursor step the cursor mapping must account for.
    newline: bool,
}

/// One laid-out unit of an input row.
#[derive(Debug)]
pub(crate) enum Segment {
    /// A word or whitespace run of the typed text.
    Text(Span<'static>),
    /// One attachment chip: its boxed label, atomic — one source
    /// character, one step, never split across rows.
    Chip(Vec<Span<'static>>),
}

impl Segment {
    /// The segment's display width.
    fn width(&self) -> usize {
        match self {
            Segment::Text(span) => span.content.width(),
            Segment::Chip(spans) => spans.iter().map(|span| span.content.width()).sum(),
        }
    }

    /// The segment's source characters — a chip is one.
    fn source(&self) -> usize {
        match self {
            Segment::Text(span) => span.content.chars().count(),
            Segment::Chip(_) => 1,
        }
    }

    /// The segment's rendered spans.
    fn spans(&self) -> Vec<Span<'static>> {
        match self {
            Segment::Text(span) => vec![span.clone()],
            Segment::Chip(spans) => spans.clone(),
        }
    }
}

/// One wrapped input row's segments: the spans, the source count, and
/// where each chip landed.
#[derive(Debug, Default)]
pub(crate) struct RowSegs {
    spans: Vec<Span<'static>>,
    source: usize,
    chips: Vec<(usize, usize, usize)>,
}

/// Word-wraps laid-out segments to a display width, mirroring
/// [`wrap_line`]: a whitespace run never opens a row, an overflowing
/// one is dropped, a word wider than the row hard-splits. A chip moves
/// whole instead of splitting.
pub(crate) fn wrap_segments(segments: Vec<Segment>, width: usize) -> Vec<RowSegs> {
    let mut rows: Vec<RowSegs> = Vec::new();
    let mut row = RowSegs::default();
    let mut row_width = 0usize;
    for segment in segments {
        let seg_width = segment.width();
        match &segment {
            Segment::Text(span) if span.content.chars().all(char::is_whitespace) => {
                if row_width == 0 {
                    // No leading whitespace on a row, but its source
                    // characters stay counted: the cursor mapping walks
                    // the input's char indices, and a dropped run here
                    // would leave everything after it unplaceable.
                    row.source += segment.source();
                    continue;
                }
                if row_width + seg_width <= width {
                    row_width += seg_width;
                    row.spans.extend(segment.spans());
                    row.source += segment.source();
                } else {
                    // Overflowing whitespace is dropped; the next word
                    // breaks. Its characters stay counted, for the same
                    // cursor-mapping reason as the leading run above.
                    row.source += segment.source();
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
            }
            Segment::Text(span) => {
                // Hard-split a word wider than the row into
                // display-width pieces.
                let mut piece = String::new();
                let mut piece_width = 0usize;
                for ch in span.content.chars() {
                    let char_width = ch.width().unwrap_or(0);
                    if piece_width + char_width > width && !piece.is_empty() {
                        if row_width + piece_width > width {
                            rows.push(std::mem::take(&mut row));
                            row_width = 0;
                        }
                        row.source += piece.chars().count();
                        row.spans
                            .push(Span::styled(std::mem::take(&mut piece), span.style));
                        row_width += piece_width;
                        piece_width = 0;
                    }
                    piece.push(ch);
                    piece_width += char_width;
                }
                if !piece.is_empty() {
                    if row_width + piece_width > width {
                        rows.push(std::mem::take(&mut row));
                        row_width = 0;
                    }
                    row.source += piece.chars().count();
                    row.spans.push(Span::styled(piece, span.style));
                    row_width += piece_width;
                }
            }
            Segment::Chip(_) => {
                if row_width > 0 && row_width + seg_width > width {
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
                row.chips.push((row.source, row_width, seg_width));
                row_width += seg_width;
                row.source += segment.source();
                row.spans.extend(segment.spans());
                if seg_width > width {
                    // Wider than the row alone: it overflows and the
                    // next segment starts fresh.
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
            }
        }
    }
    rows.push(row);
    rows
}

/// A paste chip's inline label: `▣1 5L` — the anchor's one-based id
/// and the payload's line count.
pub(crate) fn chip_label(paste: &str, index: usize) -> String {
    format!("▣{} {}L", index + 1, paste.lines().count())
}

/// An image chip's inline label: `▣1 PNG 12.3 KB` — the anchor's id,
/// the format, and the decoded size.
pub(crate) fn image_chip_label(image: &Image, index: usize) -> String {
    format!(
        "▣{} {} {}",
        index + 1,
        media_type_label(image.media_type),
        format_bytes(image.data.len() * 3 / 4)
    )
}

/// The format's short tag, the way a file extension reads.
pub(crate) fn media_type_label(media_type: rhema_core::ImageMediaType) -> &'static str {
    match media_type {
        rhema_core::ImageMediaType::Png => "PNG",
        rhema_core::ImageMediaType::Jpeg => "JPEG",
        rhema_core::ImageMediaType::Gif => "GIF",
        rhema_core::ImageMediaType::Webp => "WEBP",
    }
}

/// A decoded byte count as a size label: bytes under a kibibyte, else
/// KB or MB with one decimal — the precision a screenshot's size reads
/// at.
pub(crate) fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    }
}

/// One chip's span: the label padded one cell each side on a
/// surface-colored card — a thing apart from the typed text around it,
/// no border glyphs needed.
pub(crate) fn chip_spans(label: String, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!(" {label} "),
        Style::default().fg(theme.info).bg(theme.surface),
    )]
}

/// Maps the cursor's source index onto (row, column) of the layout.
/// Columns are display columns of the row's text — the render adds the
/// marker's two cells. Chips passed on the way widen the column; a
/// cursor sitting at a chip's source position lands on its first cell,
/// so the whole label crosses in one step.
pub(crate) fn cursor_at_rows(layout: &[InputRow], cursor: usize) -> Option<(usize, usize)> {
    let mut consumed = 0usize;
    for (index, row) in layout.iter().enumerate() {
        if cursor >= consumed && cursor <= consumed + row.source_chars {
            let in_row = cursor - consumed;
            let mut col = in_row;
            for (source, start, width) in &row.chips {
                if in_row == *source {
                    return Some((index, *start));
                }
                if in_row > *source {
                    col += width - 1;
                }
            }
            return Some((index, col));
        }
        consumed += row.source_chars + usize::from(row.newline);
    }
    None
}

/// The input box's visual rows: every logical line wraps as before, and
/// each attachment anchor renders as its inline chip — id and size —
/// exactly where the paste landed. A chip is one character to the
/// cursor, wrapping, and backspace alike, whatever its label's width.
/// Shared by the render and the height math so both always agree.
pub(crate) fn input_layout(
    input: &str,
    chips: &[Chip],
    width: usize,
    theme: &Theme,
) -> Vec<InputRow> {
    let mut rows: Vec<InputRow> = Vec::new();
    let text_width = width.saturating_sub(2).max(1);
    let lines: Vec<&str> = input.split('\n').collect();
    // The anchor index runs across the whole input: a chip's id and
    // payload follow their position in `input`, newline or not.
    let mut anchor = 0usize;
    for (li, line) in lines.iter().enumerate() {
        // The logical line lays out into segments: word and whitespace
        // runs, and each anchor as one atomic chip.
        let mut segments: Vec<Segment> = Vec::new();
        for (pi, piece) in line.split(crate::screens::ChatState::MARK).enumerate() {
            if pi > 0 {
                segments.push(Segment::Chip(chip_spans(
                    chips
                        .get(anchor)
                        .map(|chip| match chip {
                            Chip::Paste(paste) => chip_label(paste, anchor),
                            Chip::Image(image) => image_chip_label(image, anchor),
                        })
                        // An anchor without a chip cannot be minted; the
                        // label still names the slot so the cursor math
                        // and the render agree.
                        .unwrap_or_else(|| chip_label("", anchor)),
                    theme,
                )));
                anchor += 1;
            }
            // Split the piece into maximal word and whitespace runs, as
            // `wrap_line` would see them.
            let mut current = String::new();
            let mut current_space: Option<bool> = None;
            let flush = |current: &mut String, segments: &mut Vec<Segment>| {
                if !current.is_empty() {
                    segments.push(Segment::Text(Span::raw(std::mem::take(current))));
                }
            };
            for ch in piece.chars() {
                let space = ch.is_whitespace();
                if Some(space) != current_space && !current.is_empty() {
                    flush(&mut current, &mut segments);
                }
                current_space = Some(space);
                current.push(ch);
            }
            flush(&mut current, &mut segments);
        }
        for row in wrap_segments(segments, text_width) {
            let lead = if rows.is_empty() { "❯ " } else { "  " };
            let mut spans = vec![Span::raw(lead).fg(theme.text3)];
            spans.extend(row.spans);
            rows.push(InputRow {
                source_chars: row.source,
                chips: row.chips,
                marker: true,
                newline: false,
                line: Line::from(spans),
            });
        }
        // Every logical line pushes at least one row: its last row ends
        // in a newline unless it is the input's final line.
        if let Some(row) = rows.last_mut() {
            row.newline = li + 1 < lines.len();
        }
    }
    rows
}

/// The input box's total height: borders plus the layout rows, growing
/// with the text up to the cap. An empty input is one row.
pub(crate) fn input_rows(input: &str, chips: &[Chip], width: u16, theme: &Theme) -> u16 {
    let width = width.saturating_sub(2).max(1) as usize;
    (input_layout(input, chips, width, theme)
        .len()
        .clamp(1, INPUT_MAX_CONTENT_ROWS)
        + 2) as u16
}
