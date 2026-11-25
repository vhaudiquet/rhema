//! The transcript pane: partitioning into blocks, block rendering through
//! the wrap cache, and the per-line rendering underneath.

use crate::paste::PasteSpan;

use super::input::{chip_label, chip_spans, image_chip_label};
use super::*;
use crate::transcript;

/// One renderable unit of the transcript, the granularity the wrap
/// cache works at: one markdown response, one prompt, a tight run of
/// tool calls, a notice, or a system markdown block. Partitioning is
/// prefix-stable, appending lines extends the last block or adds one,
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
    /// A maximal run of session notifications: one left-aligned card
    /// per finish, stacked tight.
    Notification,
    /// A UI notice.
    Notice,
    /// A system-generated markdown block.
    Markdown,
}

/// A block: a half-open line range and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptBlock {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: Kind,
}

/// A graphical bubble's geometry inside its block's rendered rows:
/// where its cap row sits, how tall the card runs, and where its left
/// edge sits in cells. The transcript render turns these into the arc
/// placements' screen coordinates; the wrap cache carries them beside
/// the rows they describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BubbleGeom {
    /// The card's cap row, counted from its block's first rendered row.
    pub(crate) first_row: usize,
    /// The card's row count: cap, body, foot.
    pub(crate) rows: usize,
    /// The card's left edge, in cells from the row's own start.
    pub(crate) left: usize,
    /// The card's width in cells, corner to corner.
    pub(crate) width: usize,
    /// Which fill the graphics pass paints behind the card: the
    /// prompt's accent tint, or a notification's neutral one.
    pub(crate) fill: CardFill,
}

/// Partitions the transcript lines into blocks. A whitespace-only
/// assistant line with no response behind it is noise, models emit
/// empty text parts between tool calls, and belongs to no block.
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
            transcript::Line::Notification(_) => Kind::Notification,
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
            Kind::Notification => {
                while matches!(lines.get(index), Some(transcript::Line::Notification(_))) {
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

/// The banner's update line: the startup check found a release above
/// this build. One sentence carrying the three ways to install it, in
/// the color the settings page paints the same verdict.
fn update_notice(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw("A new update is available. Go to Settings -> Update, use ").fg(theme.warn),
        Span::raw("/update").fg(theme.warn).bold(),
        Span::raw(" or ").fg(theme.warn),
        Span::raw("rhema update").fg(theme.warn).bold(),
        Span::raw(".").fg(theme.warn),
    ])
}

/// The missing tools strip as one transcript row: the dim label, the
/// tool names in the text color, and the ask that installs them.
pub(crate) fn missing_tools_line(app: &App, theme: &Theme) -> Line<'static> {
    let mut line = Line::from(Span::raw("missing tools: ").fg(theme.text3));
    for (index, tool) in app.missing_tools.iter().enumerate() {
        if index > 0 {
            line.push_span(Span::raw(", ").fg(theme.text3));
        }
        line.push_span(Span::raw(tool.clone()).fg(theme.text2));
    }
    line.push_span(Span::raw(", ask the agent to install them").fg(theme.text3));
    line
}

pub(crate) fn render_transcript(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    theme: &Theme,
    occlusions: &[Rect],
) {
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
    // The cards draw whole only when the sprites can round them: the
    // protocol, and a fill an RGB a sprite can bake in.
    let graphics = app.graphics && theme.prompt_fill_rgb().is_some();

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
    // run each end below a blank row, tool lines sit tight against
    // each other, notices and markdown take no trailing air.
    let banner = Line::from(vec![
        Span::raw("ῥ ").fg(theme.accent_text).bold(),
        Span::raw("rhema").bold(),
        // The TUI's name, lighter than the brand mark.
        Span::raw(" code").fg(theme.accent_text),
        // What tree this binary came from, floor-lit: identity,
        // not content.
        Span::raw(format!("  {}", crate::VERSION_LABEL)).fg(theme.text3),
    ]);
    let mut rows: Vec<Line> = wrap_line(&banner, width);
    // The startup check's answer, when it found a release above this
    // build: right under the banner that names the build, so the first
    // screen says there is one. It scrolls away with the banner; the
    // settings' Update page and `/update` stay.
    if app.update.update_pending() {
        rows.extend(wrap_line(&update_notice(theme), width));
    }
    // The banner always ends on a blank row, separating it (and the
    // notice when there is one) from the conversation or the tools
    // strip below.
    rows.push(Line::from(""));
    // The startup tools strip: one row under the banner naming the
    // probed tools this machine misses, and the ask that installs them.
    // It rides the transcript canvas, so it shows once with the banner
    // and scrolls away as the conversation starts; the prompt's
    // environment section carries the same facts from the first turn
    // on, and a strip pinned over the prompt would repeat every frame.
    // Passive by design: the install goes through the agent and its
    // approval gate, where a password-prompting `sudo` is a
    // conversation, not a keystroke.
    if !app.missing_tools.is_empty() {
        rows.extend(wrap_line(&missing_tools_line(app, theme), width));
        rows.push(Line::from(""));
    }
    let mut bubbles: Vec<(usize, BubbleGeom)> = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0
            && matches!(
                blocks[index - 1].kind,
                Kind::Response | Kind::User | Kind::ToolRun | Kind::Notification
            )
        {
            rows.push(Line::from(""));
        }
        let block_top = rows.len();
        if index < reusable {
            let cached = &app.transcript_cache.blocks[index];
            rows.extend(cached.rows.iter().cloned());
            for geometry in &cached.bubbles {
                bubbles.push((block_top + geometry.first_row, *geometry));
            }
        } else {
            let (rendered, geoms) = render_block(lines, block, theme, width, app.icons.robot);
            let cached = CachedBlock {
                start: block.start,
                end: block.end,
                first: lines[block.start].clone(),
                bubbles: geoms.clone(),
                rows: rendered.clone(),
            };
            let cache = &mut app.transcript_cache;
            if index < cache.blocks.len() {
                cache.blocks[index] = cached;
            } else {
                cache.blocks.push(cached);
            }
            for geometry in &geoms {
                bubbles.push((block_top + geometry.first_row, *geometry));
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
    // A selection pins the row it started on. Streaming appends rows
    // below, and following the bottom would slide the selected text up
    // under the pointer; holding the top keeps the highlight on the
    // words the drag covered. The offset stays in step with the pinned
    // top, so clearing the selection resumes from the view the user
    // saw instead of jumping back to the live tail.
    let top = match app.selection {
        Some(selection) => {
            let top = selection.viewport_top.min(max_top);
            app.chat.scroll = max_top - top;
            top
        }
        None => max_top - app.chat.scroll,
    };
    // The prompt card: the screen cells its sprites cover, restated
    // for the runner's graphics pass. The card is a nine-slice, four
    // corner arcs at one cell each, the four edges and the interior as
    // half-cell-padded strips the placements stretch, sliced against
    // the viewport, so a
    // bubble scrolled half out of the well records only the pieces
    // that fit and no sprite crosses under the input. The popups clip
    // it too: the card sits under the text layer, but a piece crossing
    // a dialog would tint it.
    app.bubble_pieces = Vec::new();
    if graphics {
        for (first_row, geometry) in &bubbles {
            let foot_row = first_row + geometry.rows.saturating_sub(1);
            // The card slices to the rows the viewport holds: a piece
            // anchors on whole cells, and every cell it claims must be
            // a well cell, the sprites are not the Paragraph, they
            // clip nothing on their own.
            let first = (*first_row).max(top);
            let last = foot_row.min(top + visible.saturating_sub(1));
            if first > last {
                continue;
            }
            let width = geometry.width as u16;
            let left = inner.x + geometry.left as u16;
            let right = left + width - 1;
            let screen_y = |row_index: usize| inner.y + (row_index - top) as u16;
            // The card's own sprite set: the prompt set at the base,
            // the notification set one width above, so both cards
            // scroll under the same placement discipline.
            let offset = geometry.fill.sprite_offset();
            let mut card: Vec<BubblePiece> = Vec::new();
            let mut edge = |sprite: u32, col: u16, row_index: usize, cols: u16, rows: u16| {
                card.push(BubblePiece {
                    sprite: sprite + offset,
                    col,
                    row: screen_y(row_index),
                    cols,
                    rows,
                    slot: 0,
                });
            };
            // The cap band: the two arcs and the rule between them.
            if *first_row >= first && *first_row <= last {
                edge(
                    crate::term_image::Corner::TopLeft.sprite_id(),
                    left,
                    *first_row,
                    1,
                    1,
                );
                edge(
                    crate::term_image::Edge::Top.sprite_id(),
                    left + 1,
                    *first_row,
                    width - 2,
                    1,
                );
                edge(
                    crate::term_image::Corner::TopRight.sprite_id(),
                    right,
                    *first_row,
                    1,
                    1,
                );
            }
            // The interior band: the fill and the two sides it runs
            // between. `foot_row` tops the frame, so the band is empty
            // on a single-row frame, saturating keeps it so.
            let body_top = (*first_row + 1).max(first);
            let body_foot = foot_row.saturating_sub(1).min(last);
            if body_top <= body_foot {
                let rows = (body_foot - body_top + 1) as u16;
                edge(
                    crate::term_image::Edge::Left.sprite_id(),
                    left,
                    body_top,
                    1,
                    rows,
                );
                edge(
                    crate::term_image::Edge::Fill.sprite_id(),
                    left + 1,
                    body_top,
                    width - 2,
                    rows,
                );
                edge(
                    crate::term_image::Edge::Right.sprite_id(),
                    right,
                    body_top,
                    1,
                    rows,
                );
            }
            // The foot band: the rule and its two arcs.
            if foot_row >= first && foot_row <= last {
                edge(
                    crate::term_image::Corner::BottomLeft.sprite_id(),
                    left,
                    foot_row,
                    1,
                    1,
                );
                edge(
                    crate::term_image::Edge::Bottom.sprite_id(),
                    left + 1,
                    foot_row,
                    width - 2,
                    1,
                );
                edge(
                    crate::term_image::Corner::BottomRight.sprite_id(),
                    right,
                    foot_row,
                    1,
                    1,
                );
            }
            for piece in card {
                let rect = Rect::new(piece.col, piece.row, piece.cols, piece.rows);
                if occlusions.iter().any(|dialog| dialog.intersects(rect)) {
                    continue;
                }
                app.bubble_pieces.push(piece);
            }
        }
    }
    // Slots ride the list's order: one per piece, stable across the
    // frames that keep the same pieces, so the runner's placement ids
    // stay put and a scroll moves placements instead of deleting and
    // re-placing them.
    for (index, piece) in app.bubble_pieces.iter_mut().enumerate() {
        piece.slot = index as u16 + 1;
    }
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
    let total = rows.len();
    frame.render_widget(
        Paragraph::new(rows)
            .scroll((top as u16, 0))
            .style(Style::default().bg(theme.sunken).fg(theme.text)),
        inner,
    );
    app.transcript_view = crate::app::selection::TranscriptView {
        area: inner,
        top,
        rows: visible_rows,
        rail: None,
    };
    if let Some(selection) = app.selection {
        crate::app::selection::highlight(frame, &app.transcript_view, selection);
    }
    // The rail draws last: over the text and the selection highlight,
    // since it is a control, not content. It is a history affordance,
    // off at the live tail, so the reading column stays clean until
    // the reader leaves it. A drag keeps it on even when the drag
    // reaches the tail: the control must not vanish under the pointer.
    if app.options.scrollbar && (app.chat.scroll > 0 || app.rail_drag) {
        app.transcript_view.rail = render_scroll_rail(frame, inner, total, visible, top, theme);
    }
}

/// The scroll rail in the transcript's right margin: one half-block
/// column whose brighter thumb marks the visible window in the whole
/// content. It only draws when the rows run past the viewport, so a
/// short conversation keeps a clean right edge, and it stays inside
/// the two cells the wrap width already leaves free, so showing it
/// never re-wraps a block. The returned rail is the pointer's hit
/// target; `None` means nothing to drag.
fn render_scroll_rail(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    visible: usize,
    top: usize,
    theme: &Theme,
) -> Option<crate::app::selection::ScrollRail> {
    // A one-row viewport cannot show a thumb that means anything.
    if area.width == 0 || visible < 2 || total <= visible {
        return None;
    }
    let rail = crate::app::selection::ScrollRail {
        area: Rect::new(area.x + area.width - 1, area.y, 1, visible as u16),
        total,
        visible,
    };
    let (thumb_row, thumb_rows) = rail.thumb(top);
    let buffer = frame.buffer_mut();
    for row in 0..visible {
        let y = area.y + row as u16;
        let in_thumb = row >= thumb_row && row < thumb_row + thumb_rows;
        let color = if in_thumb { theme.text3 } else { theme.border };
        if let Some(cell) = buffer.cell_mut((rail.area.x, y)) {
            cell.set_symbol("▐");
            cell.set_fg(color);
            // The rail is chrome, not content: a selection covering
            // it must not reverse it into a solid block.
            cell.modifier = ratatui::style::Modifier::empty();
        }
    }
    Some(rail)
}

/// Renders one block's wrapped content rows at `width`, the separators
/// between blocks live at assembly, so a block's rows are cacheable
/// regardless of what comes after it. A graphical prompt block also
/// reports its cards' geometry, in the same row coordinates the rows
/// use.
pub(crate) fn render_block(
    lines: &[transcript::Line],
    block: &TranscriptBlock,
    theme: &Theme,
    width: usize,
    robot: &'static str,
) -> (Vec<Line<'static>>, Vec<BubbleGeom>) {
    let mut bubbles: Vec<BubbleGeom> = Vec::new();
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
        Kind::User => {
            let mut rows: Vec<Line<'static>> = Vec::new();
            for (text, images, pastes) in range.iter().filter_map(|line| match line {
                transcript::Line::User {
                    text,
                    images,
                    pastes,
                } => Some((text.as_str(), images, pastes.as_slice())),
                _ => None,
            }) {
                let (mut bubble_rows, geometry) = render_user(text, images, pastes, theme, width);
                if let Some(geometry) = geometry {
                    bubbles.push(BubbleGeom {
                        first_row: rows.len() + geometry.first_row,
                        ..geometry
                    });
                }
                rows.append(&mut bubble_rows);
            }
            rows
        }
        Kind::Notification => {
            let mut rows: Vec<Line<'static>> = Vec::new();
            for text in range.iter().filter_map(|line| match line {
                transcript::Line::Notification(text) => Some(text.as_str()),
                _ => None,
            }) {
                let (mut card_rows, mut geoms) = render_notification(text, theme, width);
                for geometry in geoms.iter_mut() {
                    geometry.first_row += rows.len();
                }
                bubbles.extend(geoms);
                rows.append(&mut card_rows);
            }
            rows
        }
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
        return (raw, bubbles);
    }
    (
        raw.iter().flat_map(|line| wrap_line(line, width)).collect(),
        bubbles,
    )
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
/// with each recorded paste and image as its chip. Shared by the bubble
/// and the single-row fallback.
fn user_body(
    text: &str,
    images: &[Image],
    pastes: &[PasteSpan],
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut anchors = 0usize;
    let mut cursor = 0usize;
    // A span's bytes must land on the text's boundaries; a stale note (a
    // message edited outside the frontend) is dropped, and the payload
    // renders as plain text.
    for span in pastes {
        let Some(before) = text.get(cursor..span.start) else {
            continue;
        };
        if span.end > text.len() || !text.is_char_boundary(span.end) {
            continue;
        }
        push_text_rows(&mut rows, before);
        rows.last_mut()
            .unwrap()
            .extend(chip_spans(chip_label(span.lines, anchors), theme));
        anchors += 1;
        cursor = span.end;
    }
    push_text_rows(&mut rows, &text[cursor..]);
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

/// Appends `text` to `rows`, one row per newline.
fn push_text_rows(rows: &mut Vec<Vec<Span<'static>>>, text: &str) {
    for (index, line_text) in text.split('\n').enumerate() {
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

/// A submitted prompt as a chat bubble: right-aligned, at most three
/// quarters of the column wide, the palette's drawn rounded frame. The
/// frame carries no background, every cell stays on the page's
/// ground, so nothing bleeds around the bubble and `background:
/// native` keeps the terminal's own ground inside it. Where the
/// graphics protocol is available the runner lays the prompt card,
/// the tint, rounded by the same four anti-aliased arcs, underneath
/// the whole frame, under the text layer, so the drawn bubble gains
/// its background back without a cell painting over the page. A paste
/// renders as its chip (id and size, the body hidden).
/// The geometry rides along for the runner's card.
pub(crate) fn render_user(
    text: &str,
    images: &[Image],
    pastes: &[PasteSpan],
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Option<BubbleGeom>) {
    let body = user_body(text, images, pastes, theme);
    bubble_card(body, width, true, theme, CardFill::Prompt)
}

/// A session notification as its own card: the same bubble frame, but
/// hugging the left edge and painted with the neutral fill. The
/// alignment and the fill carry the whole read, this is neither the
/// reader's prompt nor the model's response, so the text itself
/// carries no marker and no status hue. One card per finish; a run of
/// finishes stacks them tight.
pub(crate) fn render_notification(
    text: &str,
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<BubbleGeom>) {
    let body = vec![vec![Span::styled(
        text.to_owned(),
        Style::default().fg(theme.text),
    )]];
    let (rows, geometry) = bubble_card(body, width, false, theme, CardFill::Notice);
    (rows, geometry.into_iter().collect())
}

/// Which card a bubble is: the reader's prompt, or a session
/// notification. The card the graphics pass paints under the frame,
/// and with it the sprite set the placements reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardFill {
    /// The prompt card: the palette's accent tint.
    Prompt,
    /// The notification card: the palette's neutral lift.
    Notice,
}

impl CardFill {
    /// The sprite-id offset this card's set transmits under: the
    /// prompt set sits at the base, the notification set one width
    /// above it.
    fn sprite_offset(self) -> u32 {
        match self {
            CardFill::Prompt => 0,
            CardFill::Notice => crate::term_image::NOTICE_SPRITE_OFFSET,
        }
    }
}

/// Frames body rows as a bubble: the rounded cap and foot, one pad
/// cell each side, hugging its longest row up to the quarter-share
/// cap, at the right edge for a prompt, the left for a notification.
/// An empty body still draws a frame. Returns the rows and the card
/// geometry the graphics pass paints behind them.
fn bubble_card(
    body: Vec<Vec<Span<'static>>>,
    width: usize,
    align_right: bool,
    theme: &Theme,
    fill: CardFill,
) -> (Vec<Line<'static>>, Option<BubbleGeom>) {
    if width < BUBBLE_CHROME + 1 {
        // Too narrow for a frame: the text uses the cells it has.
        let rows = body
            .into_iter()
            .flat_map(|spans| wrap_line(&Line::from(spans), width))
            .collect();
        return (rows, None);
    }
    let max_width = (width * BUBBLE_WIDTH_NUM / BUBBLE_WIDTH_DEN)
        .max(BUBBLE_CHROME + 1)
        .min(width);
    let padded = max_width - BUBBLE_CHROME;
    let wrapped: Vec<Line<'static>> = body
        .into_iter()
        .flat_map(|spans| wrap_line(&Line::from(spans), padded))
        .collect();
    let widest = wrapped.iter().map(Line::width).max().unwrap_or(0);
    let bubble_width = (widest + BUBBLE_CHROME).min(max_width);
    let content_width = bubble_width - BUBBLE_CHROME;
    let left = if align_right { width - bubble_width } else { 0 };

    let mut rows = Vec::with_capacity(wrapped.len() + 2);
    rows.push(bubble_edge(left, '╭', '╮', bubble_width, theme));
    for line in wrapped {
        let used = line.width().min(content_width);
        let mut spans = Vec::with_capacity(line.spans.len() + 4);
        if left > 0 {
            spans.push(Span::raw(" ".repeat(left)));
        }
        spans.push(Span::styled("│", bubble_edge_style(theme)));
        spans.push(Span::styled(" ", bubble_pad(theme)));
        spans.extend(line.spans.into_iter().map(|span| on_bubble(span, theme)));
        // The right padding, one cell at least: content_width - used
        // plus the pad cell before the border.
        spans.push(Span::styled(
            " ".repeat(content_width - used + 1),
            bubble_pad(theme),
        ));
        spans.push(Span::styled("│", bubble_edge_style(theme)));
        rows.push(Line::from(spans));
    }
    rows.push(bubble_edge(left, '╰', '╯', bubble_width, theme));
    let geometry = BubbleGeom {
        first_row: 0,
        rows: rows.len(),
        left,
        width: bubble_width,
        fill,
    };
    (rows, Some(geometry))
}

/// The bubble's interior pad cells: the ink only. The background
/// stays the page's, the protocol card (or nothing) sits behind it.
fn bubble_pad(theme: &Theme) -> Style {
    Style::default().fg(theme.text)
}

/// The bubble's frame ink: border glyphs on the page's own ground, no
/// fill of their own. A cell is the smallest thing a terminal paints,
/// so the card's tint stays inside the `│` cells and the frame reads as
/// an outline instead of a ring of colored cells.
fn bubble_edge_style(theme: &Theme) -> Style {
    Style::default().fg(theme.border_strong)
}

/// Puts a body span inside the frame: plain text takes the bubble's
/// ink, a chip keeps a card of its own. The light palettes give
/// `surface` and `elevated` the same value, so a chip inset against
/// the bubble uses the sunken fill to stay a box.
fn on_bubble(mut span: Span<'static>, theme: &Theme) -> Span<'static> {
    if span.style.bg == Some(theme.surface) {
        span.style = span.style.bg(theme.sunken);
        return span;
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
        transcript::Line::User {
            text,
            images,
            pastes,
        } => {
            // This arm is the fallback off the transcript's bubble path;
            // pastes collapse to their chips here too.
            let mut spans = Vec::new();
            for (index, row) in user_body(text, images, pastes, theme)
                .into_iter()
                .enumerate()
            {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(row);
            }
            Line::from(spans)
        }
        // Off the card path, selection copy reads the same rows,
        // a notification is its plain text.
        transcript::Line::Notification(text) => Line::from(Span::raw(text.clone())),
        transcript::Line::Assistant(text) => Line::from(Span::raw(text.clone())),
        transcript::Line::ToolCall {
            name,
            args,
            ok,
            code,
            ..
        } => {
            // A shell call reads as a transcript of what ran: `$ cmd`,
            // with the exit code trailing in brackets, green for zero,
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
    // The suffix is bare width, the cell it leaves for the gap is the
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
