//! Rendering only: maps the app state onto ratatui widgets. No logic —
//! every decision lives in [`crate::app`] and [`crate::screens`].

use crate::palette::Theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Screen, SettingsHit};
use crate::screens::{Chip, PROVIDERS, PickerScreen, PickerStatus, SettingsSection, format_tokens};
use crate::transcript::pretty_args;
use rhema_core::Image;
use rhema_provider::ModelCard;
use std::path::Path;

use dialogs::{
    render_approval_modal, render_confirm_quit, render_picker, render_profile_picker,
    render_resume_picker,
};
use footer::render_footer;
use input::{input_rows, render_input};
use onboarding::render_onboarding;
use overlay::render_command_palette;
use pane::render_transcript;
use settings::render_settings;
use status::{render_status, subagent_lines, thinking_rows};

mod dialogs;
mod footer;
mod input;
mod onboarding;
mod overlay;
mod pane;
mod settings;
mod status;
#[cfg(test)]
mod tests;

/// The transcript render's cache: the wrapped rows of whole blocks. A
/// cached block still holds while its line range and first line are
/// unchanged and no line inside mutated since it was drawn; the prefix
/// of blocks that holds reuses its rows instead of re-parsing markdown
/// and re-wrapping every frame.
#[derive(Debug, Default)]
pub(crate) struct WrapCache {
    /// The width the rows were wrapped at.
    width: usize,
    /// The theme the rows were styled with; the whole cache dies on a
    /// change, including a generated palette switching seeds.
    theme: Option<Theme>,
    /// The icon flavor the rows were drawn with; a flip redraws.
    icons: Option<crate::icons::Flavor>,
    /// One entry per cached block, in order.
    blocks: Vec<CachedBlock>,
}

impl WrapCache {
    /// An empty cache: the first frame renders and fills it.
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

/// One cached block: its line range, its first line (the wholesale-
/// rebuild check — a new transcript can repeat a shape but not a
/// conversation), and its wrapped rows.
#[derive(Debug)]
struct CachedBlock {
    start: usize,
    end: usize,
    first: crate::transcript::Line,
    rows: Vec<Line<'static>>,
}

/// Renders one frame.
pub fn render(app: &mut App, frame: &mut Frame) {
    // A copy, not a borrow: the render calls below need `&mut app` and
    // generated palettes hand out owned tokens.
    let theme = app.theme;
    let icons = app.icons;
    // The canvas: one background everywhere, the model output's. Nothing
    // in the chat screen sits on a different panel.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sunken).fg(theme.text)),
        frame.area(),
    );
    let mut hits = Vec::new();
    // The logo rect is re-stated every frame: the wizard's rail sets it,
    // every other screen clears it, and the runner diffs it against
    // what the terminal graphics layer holds.
    app.logo_rect = None;
    let mut onboarding_hits = Vec::new();
    match &app.screen {
        Screen::Onboarding(wizard) => render_onboarding(
            frame,
            wizard,
            icons,
            &theme,
            &mut app.logo_rect,
            &mut onboarding_hits,
        ),
        Screen::Picker(picker) => render_picker(frame, picker, icons, &theme),
        Screen::Profile(profile) => render_profile_picker(frame, profile, &theme),
        Screen::Resume(resume) => render_resume_picker(frame, resume, &theme),
        Screen::Settings(settings) => render_settings(frame, settings, &theme, icons, &mut hits),
        Screen::Chat => {
            // An error wraps, so its strip grows to the wrapped height
            // (plus the /retry hint's own row when one can run). Either
            // strip — error or spinner — carries a blank row above it,
            // so it never sits flush against the transcript's tail;
            // idle, the strip is just that single row of air between
            // transcript and input.
            let status_rows = match &app.error {
                // Mirror what `render_status` draws for an error: the
                // text wrapped one cell short (its indent cannot push a
                // wrap), the blank row above, the /retry hint, and every
                // live subagent line — jobs outlive turns, so they ride
                // under an error too.
                Some(error) => {
                    wrap_line(
                        &Line::from(error.clone()),
                        frame.area().width.saturating_sub(1) as usize,
                    )
                    .len() as u16
                        + 1
                        + u16::from(app.retryable)
                        + subagent_lines(app, &theme).len() as u16
                }
                None if app.turn.running => {
                    2 + subagent_lines(app, &theme).len() as u16
                        + thinking_rows(app, frame.area().width as usize)
                }
                // A job outliving its turn keeps its line: jobs are
                // bound to the agent's lifetime, not the turn's.
                None => 1 + subagent_lines(app, &theme).len() as u16,
            };
            let input_rows =
                input_rows(&app.chat.input, &app.chat.chips, frame.area().width, &theme);
            let [transcript_area, status_area, input_area, footer_area] = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(status_rows),
                Constraint::Length(input_rows),
                Constraint::Length(1),
            ])
            .areas(frame.area());
            render_transcript(frame, app, transcript_area, &theme);
            render_status(frame, app, status_area, &theme);
            render_input(frame, app, input_area, &theme);
            render_command_palette(frame, app, input_area, &theme);
            render_footer(frame, app, footer_area, &theme);
            if let Some(call) = &app.turn.pending_approval {
                render_approval_modal(frame, call, &theme);
            } else if app.confirm_quit {
                render_confirm_quit(frame, &theme);
            }
        }
    }
    // The hit rects land after the match: `settings` borrows the screen.
    app.settings_hits = hits;
    app.onboarding_hits = onboarding_hits;
}

/// A centered box clipped to `area`.
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

/// The flow stage's lines: the wrapped instruction — a device-grant URL
/// and its code exceed the box's width, and a cut-off code cannot be
/// typed — then the code, the URL, and the latest progress note. Each
/// part wears its own tone.
fn dialog_flow_lines(
    dialog: &crate::screens::AddInstanceDialog,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(instruction) = &dialog.flow_instruction {
        lines.extend(
            wrap_line(&Line::from(instruction.text.clone()), width)
                .into_iter()
                .map(|mut line| {
                    line.spans
                        .iter_mut()
                        .for_each(|span| span.style = span.style.fg(theme.text2));
                    line
                }),
        );
        if let Some(code) = &instruction.code {
            lines.push(Line::from(vec![
                Span::raw("code: ").fg(theme.text3),
                Span::raw(code.clone()).fg(theme.accent_text).bold(),
            ]));
        }
        if let Some(url) = &instruction.url {
            lines.extend(
                wrap_line(&Line::from(url.clone()), width)
                    .into_iter()
                    .map(|mut line| {
                        line.spans
                            .iter_mut()
                            .for_each(|span| span.style = span.style.fg(theme.text3));
                        line
                    }),
            );
        }
    }
    if let Some(waiting) = &dialog.flow_waiting {
        lines.extend(
            wrap_line(&Line::from(waiting.clone()), width)
                .into_iter()
                .map(|mut line| {
                    line.spans
                        .iter_mut()
                        .for_each(|span| span.style = span.style.fg(theme.text3));
                    line
                }),
        );
    }
    lines
}

/// One picker row: `provider/` dim, id bright, then display name, price,
/// the vision verdict, and context window dim. The verdict is tri-state
/// like the catalog's: `None` — the common case — says nothing, because
/// unknown is not a verdict.
fn model_row(
    provider: &str,
    model: &ModelCard,
    icons: &crate::icons::Icons,
    theme: &Theme,
) -> Line<'static> {
    let mut details = String::new();
    if let Some(pricing) = model.pricing {
        details.push_str(&format!(
            "${:.2}/M in · ${:.2}/M out · ",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    match model.supports_vision {
        Some(true) => details.push_str(&format!("{} vision · ", icons.vision)),
        Some(false) => details.push_str("no vision · "),
        None => {}
    }
    details.push_str(&format!("{} ctx", format_tokens(model.input_token_limit)));
    Line::from(vec![
        Span::raw(format!("{provider}/")).fg(theme.text3),
        Span::raw(model.id.clone()).bold(),
        Span::raw("  "),
        Span::raw(details).fg(theme.text3),
    ])
}

/// Greedy word wrap over styled spans. A word wider than `width` is
/// hard-split at the display-width boundary, so the returned row count is
/// exact for the given width — the transcript scroll math depends on it.
fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    // A line that already fits passes through verbatim: re-tokenizing
    // would collapse the whitespace runs that table padding and indents
    // depend on.
    let total: usize = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum();
    if total <= width && width > 0 {
        let owned: Vec<Span<'static>> = line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.to_string(), span.style))
            .collect();
        return vec![Line::from(owned)];
    }
    if width == 0 {
        let owned: Vec<Span<'static>> = line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.to_string(), span.style))
            .collect();
        return vec![Line::from(owned)];
    }
    // Flatten the spans to styled runs: maximal word and whitespace runs.
    let mut runs: Vec<(Style, String)> = Vec::new();
    for span in &line.spans {
        let mut current = String::new();
        let mut current_space = None;
        for ch in span.content.chars() {
            let space = ch.is_whitespace();
            if Some(space) != current_space && !current.is_empty() {
                runs.push((span.style, std::mem::take(&mut current)));
            }
            current_space = Some(space);
            current.push(ch);
        }
        if !current.is_empty() {
            runs.push((span.style, current));
        }
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for (style, run) in runs {
        if run.chars().all(char::is_whitespace) {
            if row_width == 0 {
                continue; // no leading whitespace on a row
            }
            let run_width = run.width();
            if row_width + run_width <= width {
                row_width += run_width;
                rows.last_mut().unwrap().push(Span::styled(run, style));
            } else {
                // Overflowing whitespace is dropped; the next word breaks.
                row_width = width;
            }
            continue;
        }
        // Hard-split a word into display-width pieces.
        let mut pieces: Vec<(String, usize)> = Vec::new();
        let mut piece = String::new();
        let mut piece_width = 0usize;
        for ch in run.chars() {
            let char_width = ch.width().unwrap_or(0);
            if piece_width + char_width > width && !piece.is_empty() {
                pieces.push((std::mem::take(&mut piece), piece_width));
                piece_width = 0;
            }
            piece.push(ch);
            piece_width += char_width;
        }
        pieces.push((piece, piece_width));
        for (index, (piece, piece_width)) in pieces.into_iter().enumerate() {
            if index > 0 || row_width + piece_width > width {
                rows.push(Vec::new());
                row_width = 0;
            }
            rows.last_mut().unwrap().push(Span::styled(piece, style));
            row_width += piece_width;
        }
    }
    rows.into_iter().map(Line::from).collect()
}
