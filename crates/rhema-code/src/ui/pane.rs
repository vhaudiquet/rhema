//! The transcript pane: partitioning into blocks, block rendering through
//! the wrap cache, and the per-line rendering underneath.

use super::input::{chip_label, chip_spans, image_chip_label};
use super::*;
use crate::transcript;

/// One renderable unit of the transcript, the granularity the wrap
/// cache works at: one markdown response, one prompt, a tight run of
/// tool calls, a notice, or a system markdown block. Partitioning is
/// prefix-stable — appending lines extends the last block or adds one,
/// never rejoins an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Consecutive assistant lines: one markdown response, streaming or
    /// done.
    Response,
    /// A user prompt.
    User,
    /// A maximal run of tool-call lines; they sit tight together.
    ToolRun,
    /// A UI notice.
    Notice,
    /// A system-generated markdown block.
    Markdown,
}

/// A block: a half-open line range and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptBlock {
    start: usize,
    end: usize,
    kind: Kind,
}

/// Partitions the transcript lines into blocks. A whitespace-only
/// assistant line with no response behind it is noise — models emit
/// empty text parts between tool calls — and belongs to no block.
pub(crate) fn partition(lines: &[transcript::Line]) -> Vec<TranscriptBlock> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let kind = match &lines[index] {
            transcript::Line::Assistant(text) => {
                if text.trim().is_empty() {
                    index += 1;
                    continue;
                }
                Kind::Response
            }
            transcript::Line::User { .. } => Kind::User,
            transcript::Line::ToolCall { .. } => Kind::ToolRun,
            transcript::Line::Notice(_) => Kind::Notice,
            transcript::Line::Markdown(_) => Kind::Markdown,
        };
        let start = index;
        index += 1;
        // A response runs while the lines stay assistant text. A tool
        // run also absorbs the model's empty text parts between calls:
        // they render as nothing and never end a run.
        match kind {
            Kind::Response => {
                while index < lines.len() && matches!(lines[index], transcript::Line::Assistant(_))
                {
                    index += 1;
                }
            }
            Kind::ToolRun => {
                while index < lines.len()
                    && match &lines[index] {
                        transcript::Line::ToolCall { .. } => true,
                        transcript::Line::Assistant(text) => text.trim().is_empty(),
                        _ => false,
                    }
                {
                    index += 1;
                }
            }
            _ => {}
        }
        blocks.push(TranscriptBlock {
            start,
            end: index,
            kind,
        });
    }
    blocks
}

pub(crate) fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };

    // Wrapping is done here rather than by Paragraph so the row count is
    // exact: the scroll offset is expressed in rendered rows.
    let width = inner.width.saturating_sub(2) as usize;
    let lines = app.transcript.lines();
    let blocks = partition(lines);

    // The cached prefix: blocks whose range, first line, and mutation
    // watermark all still agree. A width or theme change, or a rebuilt
    // transcript (every block dirty), answers for none.
    let reusable = {
        let cache = &app.transcript_cache;
        if cache.width == width
            && cache.theme == Some(*theme)
            && cache.icons == Some(app.options.icons)
        {
            blocks
                .iter()
                .zip(cache.blocks.iter())
                .take_while(|(block, cached)| {
                    block.start == cached.start
                        && block.end == cached.end
                        && block.end <= app.transcript.dirty_from()
                        && lines[block.start] == cached.first
                })
                .count()
        } else {
            0
        }
    };
    {
        let cache = &mut app.transcript_cache;
        cache.width = width;
        cache.theme = Some(*theme);
        cache.icons = Some(app.options.icons);
        cache.blocks.truncate(reusable);
    }

    // The canvas of rows: the banner, then the blocks with their
    // separators. Blocks read apart: a prompt, a response, and a tool
    // run each end below a blank row — tool lines sit tight against
    // each other, notices and markdown take no trailing air.
    let banner = [
        Line::from(vec![
            Span::raw("ῥ ").fg(theme.accent_text).bold(),
            Span::raw("rhema").bold(),
            // The TUI's name, lighter than the brand mark.
            Span::raw(" code").fg(theme.accent_text),
            // What tree this binary came from, floor-lit: identity,
            // not content.
            Span::raw(format!("  {}", crate::VERSION_LABEL)).fg(theme.text3),
        ]),
        Line::from(""),
    ];
    let mut rows: Vec<Line> = banner
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0
            && matches!(
                blocks[index - 1].kind,
                Kind::Response | Kind::User | Kind::ToolRun
            )
        {
            rows.push(Line::from(""));
        }
        if index < reusable {
            rows.extend(app.transcript_cache.blocks[index].rows.iter().cloned());
        } else {
            let rendered = render_block(lines, block, theme, width, app.icons.robot);
            let cached = CachedBlock {
                start: block.start,
                end: block.end,
                first: lines[block.start].clone(),
                rows: rendered.clone(),
            };
            let cache = &mut app.transcript_cache;
            if index < cache.blocks.len() {
                cache.blocks[index] = cached;
            } else {
                cache.blocks.push(cached);
            }
            rows.extend(rendered);
        }
    }
    if matches!(blocks.last().map(|block| block.kind), Some(Kind::User)) {
        rows.push(Line::from(""));
    }

    let visible = inner.height as usize;
    let max_top = rows.len().saturating_sub(visible);
    // PgUp has no content bound at press time, so the offset can sit
    // above the top; clamping here keeps PgDn answering immediately
    // instead of burning the surplus first.
    app.chat.scroll = app.chat.scroll.min(max_top);
    let top = max_top - app.chat.scroll;
    // The rows the frame draws, as plain text: the cells a mouse
    // selection maps onto and copies from.
    let visible_rows: Vec<String> = rows
        .iter()
        .skip(top)
        .take(visible)
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rows)
            .scroll((top as u16, 0))
            .style(Style::default().bg(theme.sunken).fg(theme.text)),
        inner,
    );
    app.transcript_view = crate::app::selection::TranscriptView {
        area: inner,
        rows: visible_rows,
    };
    if let Some(selection) = app.selection {
        crate::app::selection::highlight(frame, &app.transcript_view, selection);
    }
}

/// Renders one block's wrapped content rows at `width` — the separators
/// between blocks live at assembly, so a block's rows are cacheable
/// regardless of what comes after it.
pub(crate) fn render_block(
    lines: &[transcript::Line],
    block: &TranscriptBlock,
    theme: &Theme,
    width: usize,
    robot: &'static str,
) -> Vec<Line<'static>> {
    let range = &lines[block.start..block.end];
    let raw: Vec<Line<'static>> = match block.kind {
        Kind::Response => {
            // One markdown response, the bullet on its first row.
            let source = range
                .iter()
                .filter_map(|line| match line {
                    transcript::Line::Assistant(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            crate::markdown::render_markdown(&source, theme, width)
                .into_iter()
                .enumerate()
                .map(|(index, mut line)| {
                    if index == 0 {
                        line.spans.insert(
                            0,
                            Span::styled("● ", Style::default().fg(theme.accent_text)),
                        );
                    }
                    line
                })
                .collect()
        }
        Kind::User => range
            .iter()
            .filter_map(|line| match line {
                transcript::Line::User { text, images } => Some((text.as_str(), images)),
                _ => None,
            })
            .flat_map(|(text, images)| render_user(text, images, theme, width))
            .collect(),
        // The run's absorbed empty text parts render as nothing.
        Kind::ToolRun => range
            .iter()
            .filter_map(|line| match line {
                transcript::Line::ToolCall { args, ok, code, .. } if name_is_bash(line) => {
                    Some(tool_row(args, *ok, *code, width, theme))
                }
                transcript::Line::ToolCall { .. } => Some(render_line(line, theme, robot)),
                _ => None,
            })
            .collect(),
        Kind::Notice => range
            .iter()
            .map(|line| render_line(line, theme, robot))
            .collect(),
        Kind::Markdown => range
            .iter()
            .filter_map(|line| match line {
                transcript::Line::Markdown(source) => Some(source.as_str()),
                _ => None,
            })
            .flat_map(|source| crate::markdown::render_markdown(source, theme, width))
            .collect(),
    };
    if block.kind == Kind::User {
        // A prompt arrives already wrapped to its bubble's width and
        // filled out to the row: re-wrapping would drop the right-side
        // padding that holds the bubble against the edge.
        return raw;
    }
    raw.iter().flat_map(|line| wrap_line(line, width)).collect()
}

/// The share of the transcript width a prompt bubble may occupy: at
/// three quarters, a long prompt still reads as a message of its own,
/// with the model's output keeping the column's left side.
const BUBBLE_WIDTH_NUM: usize = 3;
/// Denominator of [`BUBBLE_WIDTH_NUM`].
const BUBBLE_WIDTH_DEN: usize = 4;
/// The cells a bubble spends on its frame: two border cells and one
/// padding cell on each side.
const BUBBLE_CHROME: usize = 4;

/// A prompt's body rows before wrapping: one span list per logical line,
/// with an embedded paste or an image as its chip. Shared by the bubble
/// and the single-row fallback.
fn user_body(text: &str, images: &[Image], theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut anchors = 0usize;
    for part in rhema_core::paste::parts(text) {
        match part {
            rhema_core::paste::Part::Text(segment) => {
                for (index, line_text) in segment.split('\n').enumerate() {
                    if index > 0 {
                        rows.push(Vec::new());
                    }
                    if !line_text.is_empty() {
                        rows.last_mut()
                            .unwrap()
                            .push(Span::raw(line_text.to_owned()));
                    }
                }
            }
            rhema_core::paste::Part::Paste(payload) => {
                rows.last_mut()
                    .unwrap()
                    .extend(chip_spans(chip_label(payload, anchors), theme));
                anchors += 1;
            }
        }
    }
    // The images ride after the text, on its last row: they have no
    // inline form to sit inside.
    for image in images {
        rows.last_mut()
            .unwrap()
            .extend(chip_spans(image_chip_label(image, anchors), theme));
        anchors += 1;
    }
    rows
}

/// A submitted prompt as a chat bubble: right-aligned, at most three
/// quarters of the column wide, on a rounded elevated fill so it reads
/// apart from the model's output. An embedded paste renders as its chip
/// (id and size, the body hidden).
pub(crate) fn render_user(
    text: &str,
    images: &[Image],
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let body = user_body(text, images, theme);
    if width < BUBBLE_CHROME + 1 {
        // Too narrow for a frame: the text uses the cells it has.
        return body
            .into_iter()
            .flat_map(|spans| wrap_line(&Line::from(spans), width))
            .collect();
    }
    let max_width = (width * BUBBLE_WIDTH_NUM / BUBBLE_WIDTH_DEN)
        .max(BUBBLE_CHROME + 1)
        .min(width);
    let padded = max_width - BUBBLE_CHROME;
    let wrapped: Vec<Line<'static>> = body
        .into_iter()
        .flat_map(|spans| wrap_line(&Line::from(spans), padded))
        .collect();
    // The bubble hugs its longest row, up to the quarter-share cap. An
    // empty prompt still draws a frame.
    let widest = wrapped.iter().map(Line::width).max().unwrap_or(0);
    let bubble_width = (widest + BUBBLE_CHROME).min(max_width);
    let content_width = bubble_width - BUBBLE_CHROME;
    let left = width - bubble_width;

    let mut rows = Vec::with_capacity(wrapped.len() + 2);
    rows.push(bubble_edge(left, '╭', '╮', bubble_width, theme));
    for line in wrapped {
        let used = line.width().min(content_width);
        let mut spans = Vec::with_capacity(line.spans.len() + 4);
        if left > 0 {
            spans.push(Span::raw(" ".repeat(left)));
        }
        spans.push(Span::styled("│", bubble_edge_style(theme)));
        spans.push(Span::styled(" ", bubble_fill(theme)));
        spans.extend(line.spans.into_iter().map(|span| on_bubble(span, theme)));
        // The right padding, one cell at least: content_width - used
        // plus the pad cell before the border.
        spans.push(Span::styled(
            " ".repeat(content_width - used + 1),
            bubble_fill(theme),
        ));
        spans.push(Span::styled("│", bubble_edge_style(theme)));
        rows.push(Line::from(spans));
    }
    rows.push(bubble_edge(left, '╰', '╯', bubble_width, theme));
    rows
}

/// The bubble's fill: content ink on the lifted surface.
fn bubble_fill(theme: &Theme) -> Style {
    Style::default().bg(theme.elevated).fg(theme.text)
}

/// The bubble's frame ink.
fn bubble_edge_style(theme: &Theme) -> Style {
    Style::default().bg(theme.elevated).fg(theme.border_strong)
}

/// Puts a body span on the bubble's fill: plain text takes the bubble's
/// ink and background, a chip keeps a card of its own. The light
/// palettes give `surface` and `elevated` the same value, so a chip
/// inset against the bubble uses the sunken fill to stay a box.
fn on_bubble(mut span: Span<'static>, theme: &Theme) -> Span<'static> {
    if span.style.bg == Some(theme.surface) {
        span.style = span.style.bg(theme.sunken);
        return span;
    }
    if span.style.bg.is_none() {
        span.style = span.style.bg(theme.elevated);
    }
    if span.style.fg.is_none() {
        span.style = span.style.fg(theme.text);
    }
    span
}

/// One horizontal edge of the bubble: the corners and the rule between
/// them, right-aligned to `width` by the caller's left pad.
fn bubble_edge(
    left: usize,
    start: char,
    end: char,
    bubble_width: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(2);
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.push(Span::styled(
        format!("{start}{}{end}", "─".repeat(bubble_width - 2)),
        bubble_edge_style(theme),
    ));
    Line::from(spans)
}

pub(crate) fn render_line(
    line: &transcript::Line,
    theme: &Theme,
    robot: &'static str,
) -> Line<'static> {
    match line {
        transcript::Line::User { text, images } => {
            // This arm is the fallback off the transcript's bubble path;
            // embedded pastes still collapse to their chips here.
            let mut spans = Vec::new();
            for (index, row) in user_body(text, images, theme).into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(row);
            }
            Line::from(spans)
        }
        transcript::Line::Assistant(text) => Line::from(Span::raw(text.clone())),
        transcript::Line::ToolCall {
            name,
            args,
            ok,
            code,
            ..
        } => {
            // A shell call reads as a transcript of what ran: `$ cmd`,
            // with the exit code trailing in brackets — green for zero,
            // red for anything else.
            if name == "bash" {
                let mut spans = vec![Span::raw("$ ").fg(theme.text3)];
                spans.push(Span::raw(transcript::describe(name, args)));
                match code {
                    Some(0) => spans.push(Span::raw(" [0]").fg(theme.ok)),
                    Some(n) => spans.push(Span::raw(format!(" [{n}]")).fg(theme.error)),
                    // No exit code: still running, or rejected without
                    // reaching a shell.
                    None if *ok == Some(false) => spans.push(Span::raw(" ✗").fg(theme.error)),
                    None => {}
                }
                return Line::from(spans);
            }
            // A subagent call reads as delegation: the robot badge, the
            // mark, the subagent and what it was asked.
            if name == "subagent" {
                let mut spans = vec![Span::raw(format!("{robot} ")).fg(theme.text3)];
                match ok {
                    Some(true) => spans.push(Span::raw("✓ ").fg(theme.ok)),
                    Some(false) => spans.push(Span::raw("✗ ").fg(theme.error)),
                    // Still running: keep the mark column aligned.
                    None => spans.push(Span::raw("  ")),
                }
                spans.push(Span::raw(transcript::describe(name, args)));
                return Line::from(spans);
            }
            let mut spans = vec![Span::raw("⌁ ").fg(theme.text3)];
            match ok {
                Some(true) => spans.push(Span::raw("✓ ").fg(theme.ok)),
                Some(false) => spans.push(Span::raw("✗ ").fg(theme.error)),
                // Still running: keep the mark column aligned.
                None => spans.push(Span::raw("  ")),
            }
            spans.push(Span::raw(transcript::describe(name, args)));
            Line::from(spans)
        }
        transcript::Line::Notice(text) => {
            Line::from(Span::raw(text.clone()).fg(theme.info).italic())
        }
        // Markdown blocks never reach here: they expand in the
        // transcript loop.
        transcript::Line::Markdown(_) => Line::from(""),
    }
}

/// Whether a tool line is the live shell tool.
pub(crate) fn name_is_bash(line: &transcript::Line) -> bool {
    matches!(line, transcript::Line::ToolCall { name, .. } if name == "bash")
}

/// One tool-call row, built at the terminal's width: `$ command` with
/// the exit code flush right. The command ellipsizes to what fits; a
/// call still running or rejected before a shell has no code to show.
pub(crate) fn tool_row(
    args: &str,
    ok: Option<bool>,
    code: Option<i32>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let command = transcript::describe("bash", args);
    let prefix_width = 2; // "$ "
    let suffix: Option<(String, ratatui::style::Color)> = match code {
        Some(0) => Some(("[0]".to_owned(), theme.ok)),
        Some(n) => Some((format!("[{n}]"), theme.error)),
        // Rejected without reaching a shell: no code exists.
        None if ok == Some(false) => Some(("✗".to_owned(), theme.error)),
        None => None,
    };
    // Cells, not chars: the row is drawn by terminal cells, so a wide
    // glyph in the command or the code shifts the alignment otherwise.
    // The suffix is bare width — the cell it leaves for the gap is the
    // budget's +1, counted once.
    let suffix_width = suffix.as_ref().map_or(0, |(text, _)| text.width());
    let command = transcript::truncate_width(
        &command,
        width.saturating_sub(prefix_width + suffix_width + 1),
    );
    let mut spans = vec![Span::raw("$ ").fg(theme.text3)];
    spans.push(Span::raw(command.clone()));
    if let Some((text, color)) = suffix {
        let gap = width
            .saturating_sub(prefix_width + command.width() + suffix_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    Line::from(spans)
}
