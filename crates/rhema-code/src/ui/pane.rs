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
    frame.render_widget(
        Paragraph::new(rows)
            .scroll((top as u16, 0))
            .style(Style::default().bg(theme.sunken).fg(theme.text)),
        inner,
    );
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
            .flat_map(|(text, images)| render_user(text, images, theme))
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
    raw.iter().flat_map(|line| wrap_line(line, width)).collect()
}

/// A multi-line prompt renders as the lines it is: a raw \n inside one
/// row's spans would vanish. The prompt glyph marks only the first row;
/// the rest speak for themselves. An embedded paste renders as its
/// chip — id and size, the body hidden.
pub(crate) fn render_user(text: &str, images: &[Image], theme: &Theme) -> Vec<Line<'static>> {
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
    rows.into_iter()
        .enumerate()
        .map(|(index, mut spans)| {
            if index == 0 {
                spans.insert(0, Span::raw("❯ ").fg(theme.text3));
            }
            Line::from(spans)
        })
        .collect()
}

pub(crate) fn render_line(
    line: &transcript::Line,
    theme: &Theme,
    robot: &'static str,
) -> Line<'static> {
    match line {
        transcript::Line::User { text, images } => {
            // Embedded pastes collapse to their chips here too — this
            // arm is the fallback off the transcript's multi-row path.
            let mut spans = vec![Span::raw("❯ ").fg(theme.text3)];
            let mut anchors = 0usize;
            for part in rhema_core::paste::parts(text) {
                match part {
                    rhema_core::paste::Part::Text(text) => {
                        spans.push(Span::raw(text.replace('\n', " ")));
                    }
                    rhema_core::paste::Part::Paste(payload) => {
                        spans.extend(chip_spans(chip_label(payload, anchors), theme));
                        anchors += 1;
                    }
                }
            }
            for image in images {
                spans.extend(chip_spans(image_chip_label(image, anchors), theme));
                anchors += 1;
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
    let suffix_width = suffix
        .as_ref()
        .map_or(0, |(text, _)| text.chars().count() + 1);
    let command = transcript::truncate_chars(
        &command,
        width.saturating_sub(prefix_width + suffix_width + 1),
    );
    let mut spans = vec![Span::raw("$ ").fg(theme.text3)];
    spans.push(Span::raw(command.clone()));
    if let Some((text, color)) = suffix {
        let gap = width
            .saturating_sub(prefix_width + command.chars().count() + suffix_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    Line::from(spans)
}
