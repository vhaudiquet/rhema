//! Rendering only: maps the app state onto ratatui widgets. No logic —
//! every decision lives in [`crate::app`] and [`crate::screens`].

use crate::palette::Theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Screen};
use crate::screens::{COMING_SOON, PROVIDERS, PickerScreen, PickerStatus};
use crate::transcript::{self, pretty_args};
use rhema_provider_google::GeminiModel;

/// Renders one frame.
pub fn render(app: &App, frame: &mut Frame) {
    let theme = app.theme;
    // The canvas: one background everywhere, the model output's. Nothing
    // in the chat screen sits on a different panel.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sunken).fg(theme.text)),
        frame.area(),
    );
    match &app.screen {
        Screen::Login(login) => render_login(frame, login, theme),
        Screen::Picker(picker) => render_picker(frame, picker, theme),
        Screen::Chat => {
            // An error wraps, so the status strip grows to its wrapped
            // height; every other strip is fixed.
            let status_rows = app
                .error
                .as_ref()
                .map(|error| {
                    wrap_line(&Line::from(error.clone()), frame.area().width as usize).len() as u16
                })
                .unwrap_or(1)
                .max(1);
            let [transcript_area, status_area, input_area, footer_area] = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(status_rows),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(frame.area());
            render_transcript(frame, app, transcript_area, theme);
            render_status(frame, app, status_area, theme);
            render_input(frame, app, input_area, theme);
            render_command_palette(frame, app, input_area, theme);
            render_footer(frame, app, footer_area, theme);
            if let Some(call) = &app.pending_approval {
                render_approval_modal(frame, call, theme);
            } else if app.confirm_quit {
                render_confirm_quit(frame, theme);
            }
        }
    }
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

fn render_login(frame: &mut Frame, login: &crate::screens::LoginScreen, theme: &Theme) {
    let area = centered(58, 13, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface).fg(theme.text))
        .title(" rhema — sign in ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [providers_area, input_area, status_area] = Layout::vertical([
        Constraint::Length(PROVIDERS.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let rows: Vec<ListItem> = PROVIDERS
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut spans: Vec<Span> = if index == login.selected {
                vec![Span::raw("❯ ")]
            } else {
                vec![Span::raw("  ")]
            };
            if entry.selectable {
                spans.push(Span::raw(entry.name).bold());
            } else {
                spans.push(Span::raw(entry.name).fg(theme.text3));
                spans.push(Span::raw("  "));
                spans.push(Span::raw(COMING_SOON).fg(theme.text3));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(login.selected));
    frame.render_stateful_widget(List::new(rows), providers_area, &mut state);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(login.input_label()),
            Span::raw(login.masked_input()).bold(),
        ])),
        input_area,
    );

    let status = match &login.status {
        crate::screens::LoginStatus::Idle => Line::from(login.input_hint()).fg(theme.text3),
        crate::screens::LoginStatus::Validating => Line::from("validating key…").fg(theme.text3),
        crate::screens::LoginStatus::Error(message) => Line::from(message.clone()).red(),
    };
    frame.render_widget(Paragraph::new(status), status_area);
}

fn render_picker(frame: &mut Frame, picker: &PickerScreen, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface).fg(theme.text))
        .title(" select a model ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if picker.status != PickerStatus::Ready {
        let message = match &picker.status {
            PickerStatus::Loading => Line::from("loading models…").fg(theme.text3),
            // The body below renders only in the Ready state.
            PickerStatus::Ready => Line::from(""),
            PickerStatus::Error(message) => Line::from(vec![
                Span::raw(message.clone()).red(),
                Span::raw("  "),
                Span::raw("press r to retry").fg(theme.text3),
            ]),
        };
        frame.render_widget(
            Paragraph::new(message).block(Block::default().padding(Padding::new(1, 1, 1, 0))),
            inner,
        );
        return;
    }

    // The list and the footer must not share a row: a scrolled-to-bottom
    // list draws its highlight on the last inner row, exactly where the
    // footer paints.
    let [list_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    let filtered = picker.filtered();
    let rows: Vec<ListItem> = filtered
        .iter()
        .map(|model| ListItem::new(model_row(picker.provider, model, theme)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    // Without an explicit highlight the selection is invisible: ListState
    // only offsets the scroll, the marker and style are opt-in.
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(theme.elevated))
            .style(Style::default().bg(theme.surface).fg(theme.text)),
        list_area,
        &mut state,
    );

    let footer = Line::from(vec![
        Span::raw("filter: "),
        Span::raw(picker.filter.clone()).bold(),
        Span::raw("▌").bold(),
        Span::raw("  "),
        Span::raw(format!("{} models", filtered.len())).fg(theme.text3),
        Span::raw("  "),
        Span::raw("type to filter · enter choose · esc clear/quit").fg(theme.text3),
    ]);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };

    // Wrapping is done here rather than by Paragraph so the row count is
    // exact: the scroll offset is expressed in rendered rows.
    let width = inner.width as usize;
    // The banner is the conversation's first content, not chrome: it
    // scrolls away with the history and shares the transcript background.
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw("ῥ ").fg(theme.accent_text).bold(),
            Span::raw("rhema").bold(),
        ]),
        Line::from(
            Span::raw(format!(
                "{} · {} · /help for commands",
                app.config.model.as_deref().unwrap_or("no model"),
                app.options.workspace.display()
            ))
            .fg(theme.text3),
        ),
        Line::from(""),
    ];
    // Consecutive assistant lines form one markdown response and render
    // through the parser; everything else renders as-is. The bullet marks
    // the first row of each response.
    let mut group: Vec<String> = Vec::new();
    let mut marked = false;
    for line in app.transcript.lines() {
        match line {
            transcript::Line::Assistant(text) => group.push(text.clone()),
            other => {
                flush_response_group(&mut lines, &mut group, &mut marked, app.theme);
                marked = false;
                lines.push(render_line(other, theme));
            }
        }
    }
    flush_response_group(&mut lines, &mut group, &mut marked, app.theme);
    let wrapped: Vec<Line> = lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect();
    let visible = inner.height as usize;
    let max_top = wrapped.len().saturating_sub(visible);
    let top = max_top.saturating_sub(app.chat.scroll.min(max_top));
    frame.render_widget(
        Paragraph::new(wrapped)
            .scroll((top as u16, 0))
            .style(Style::default().bg(theme.sunken).fg(theme.text)),
        inner,
    );
}

/// One picker row: `provider/` dim, id bright, then display name, price,
/// and context window dim.
fn model_row(provider: &str, model: &GeminiModel, theme: &Theme) -> Line<'static> {
    let mut details = String::new();
    if let Some(pricing) = model.pricing() {
        details.push_str(&format!(
            "${:.2}/M in · ${:.2}/M out · ",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    details.push_str(&format!("{} ctx", format_tokens(model.input_token_limit)));
    Line::from(vec![
        Span::raw(format!("{provider}/")).fg(theme.text3),
        Span::raw(model.id.clone()).bold(),
        Span::raw("  "),
        Span::raw(details).fg(theme.text3),
    ])
}

/// Humanizes a token count: 1_048_576 → "1.0M", 131_072 → "131k", none →
/// "?".
fn format_tokens(limit: Option<u64>) -> String {
    match limit {
        None => "?".to_owned(),
        Some(limit) if limit >= 1_000_000 => {
            format!("{:.1}M", limit as f64 / 1_000_000.0)
        }
        Some(limit) if limit >= 10_000 => format!("{}k", limit / 1_000),
        Some(limit) => format!("{limit}"),
    }
}

/// Greedy word wrap over styled spans. A word wider than `width` is
/// hard-split at the display-width boundary, so the returned row count is
/// exact for the given width — the transcript scroll math depends on it.
fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
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

/// Renders one accumulated assistant response as markdown into `lines`,
/// marking its first row. An empty group is a no-op.
fn flush_response_group(
    lines: &mut Vec<Line<'static>>,
    group: &mut Vec<String>,
    marked: &mut bool,
    theme: &Theme,
) {
    if group.is_empty() {
        return;
    }
    let source = group.join("\n");
    group.clear();
    let rendered = crate::markdown::render_markdown(&source, theme);
    for (index, mut line) in rendered.into_iter().enumerate() {
        if index == 0 && !*marked {
            *marked = true;
            line.spans.insert(
                0,
                Span::styled("● ", Style::default().fg(theme.accent_text)),
            );
        }
        lines.push(line);
    }
}

fn render_line(line: &transcript::Line, theme: &Theme) -> Line<'static> {
    match line {
        transcript::Line::User(text) => Line::from(vec![
            Span::raw("❯ ").fg(theme.text3),
            Span::raw(text.clone()),
        ]),
        transcript::Line::Assistant(text) => Line::from(Span::raw(text.clone())),
        transcript::Line::ToolCall { name, args, ok, .. } => {
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
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let width = area.width as usize;
    let mut lines: Vec<Line> = if let Some(error) = &app.error {
        wrap_line(&Line::from(error.clone()), width)
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.error).bold()))
            .collect()
    } else if app.running {
        // A square rotating through its quadrants, animated off the
        // running turn's clock — no timer machinery of its own.
        const SPINNER: [char; 4] = ['▖', '▘', '▝', '▗'];
        let frame = app
            .turn_started
            .map(|started| (started.elapsed().as_millis() / 120) as usize % SPINNER.len())
            .unwrap_or(0);
        let elapsed = app
            .turn_started
            .map(|started| format!(" · {}s", started.elapsed().as_secs()))
            .unwrap_or_default();
        vec![Line::from(vec![
            Span::styled(
                SPINNER[frame].to_string(),
                Style::default().fg(theme.running),
            ),
            Span::raw(" thinking").fg(theme.running),
            Span::raw(elapsed).fg(theme.text3),
            Span::raw(" · esc declines pending tools").fg(theme.text3),
        ])]
    } else {
        vec![Line::from("")]
    };
    if app.chat.scroll > 0 && app.error.is_none() {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::raw("  ·  viewing history · pgdn: latest").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The dim telemetry strip under the input: last turn's tokens, and the
/// history-pinned indicator.
/// The completion popup: while the input names commands, the matches
/// float above it — full terminal width, the shared background, no panel.
/// The command reads in accent, the description dim, and the highlighted
/// row carries the elevated tone. Tab completes, enter runs.
fn render_command_palette(frame: &mut Frame, app: &App, input_area: Rect, theme: &Theme) {
    let candidates = crate::screens::filtered_commands(&app.chat.input);
    if candidates.is_empty() {
        return;
    }
    let visible = candidates.len().min(6) as u16;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(visible),
        width: input_area.width,
        height: visible,
    };
    // Clear erases the transcript beneath; the well tone is painted back
    // so the rows sit on the shared background.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.sunken)),
        area,
    );

    let selected = app.chat.palette_index.min(candidates.len() - 1);
    let rows: Vec<Line> = candidates
        .iter()
        .take(visible as usize)
        .enumerate()
        .map(|(index, (name, description))| {
            let row_bg = if index == selected {
                Style::default().bg(theme.elevated)
            } else {
                Style::default().bg(theme.sunken)
            };
            Line::from(vec![
                Span::styled(name.to_string(), row_bg.fg(theme.accent_text)),
                Span::styled(format!("  {description}"), row_bg.fg(theme.text3)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), area);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Always the same three facts: model, context footprint against the
    // model's window, and the session's accumulated cost. Key hints live
    // in /help, not here.
    let mut parts =
        vec![Span::raw(app.config.model.as_deref().unwrap_or("no model")).fg(theme.text2)];
    let used = app.usage.map(|u| u64::from(u.input_tokens)).unwrap_or(0);
    let mut context = format_tokens(Some(used));
    if let Some(limit) = app.current_context_window() {
        context.push_str(&format!("/{}", format_tokens(Some(limit))));
    }
    parts.push(Span::raw(format!(" · {context}")).fg(theme.text3));
    let cost = if app.session_cost >= 0.01 || app.session_cost == 0.0 {
        format!("${:.2}", app.session_cost)
    } else {
        format!("${:.4}", app.session_cost)
    };
    parts.push(Span::raw(format!(" · {cost}")).fg(theme.text3));
    if app.chat.scroll > 0 && app.error.is_none() {
        parts.push(Span::raw(" · viewing history").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(Line::from(parts).left_aligned()), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // The input is the one interactive element, so it earns the accent;
    // while a turn runs it calms to a hairline.
    let border = if app.running {
        theme.border_strong
    } else {
        theme.accent
    };
    // No fill of its own: the input sits on the same canvas as everything
    // else; only the border marks it.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Single-line input: a window of the text around the cursor, so the
    // cursor is always visible.
    let width = inner.width.saturating_sub(2) as usize;
    let prompt = "❯ ";
    let max_len = width.saturating_sub(prompt.chars().count());
    let chars: Vec<char> = app.chat.input.chars().collect();
    let start = app.chat.cursor.saturating_sub(max_len.saturating_sub(1));
    let visible: String = chars
        .iter()
        .skip(start)
        .take(max_len)
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    // The prompt stays visible even with the placeholder up, so the box
    // always reads as an input.
    let content = if visible.is_empty() && !app.running {
        Line::from(vec![
            Span::raw(prompt).fg(theme.text3),
            Span::raw("ask anything · /help for commands").fg(theme.text3),
        ])
    } else {
        Line::from(vec![Span::raw(prompt).fg(theme.text3), Span::raw(visible)])
    };
    frame.render_widget(Paragraph::new(content), inner);
    if !app.running {
        frame.set_cursor_position(Position {
            x: inner.x + prompt.chars().count() as u16 + (app.chat.cursor - start) as u16,
            y: inner.y,
        });
    }
}

fn render_approval_modal(frame: &mut Frame, call: &rhema_core::ToolCall, theme: &Theme) {
    let area = centered(64, 12, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(" approve tool call ")
        .border_style(Style::default().fg(theme.warn));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::raw(call.name.clone()).bold()),
            Line::from(""),
            Line::from(pretty_args(&call.args)),
        ])
        .block(Block::default().padding(Padding::horizontal(1))),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from("y allow · anything else declines").fg(theme.text3)),
        footer_area,
    );
}

fn render_confirm_quit(frame: &mut Frame, theme: &Theme) {
    let area = centered(34, 3, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_strong))
        .style(Style::default().bg(theme.surface).fg(theme.text))
        .title(" quit rhema? ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("y").bold(),
            Span::raw(" quit · "),
            Span::raw("n").bold(),
            Span::raw(" stay"),
        ])),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::DARK;
    use crate::screens::{PickerPurpose, PickerScreen};
    use ratatui::{Terminal, backend::TestBackend};
    use rhema_provider_google::GeminiModel;

    fn model(id: &str) -> GeminiModel {
        GeminiModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec!["generateContent".to_owned()],
        }
    }

    /// The selection must be visible: the highlighted row carries the
    /// marker, the others do not.
    #[test]
    fn the_selected_picker_row_shows_the_marker() {
        let picker = PickerScreen::new(
            vec![model("model-a"), model("model-b")],
            PickerPurpose::Initial,
            "google",
        );
        assert_eq!(picker.selected, 0);

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker, &DARK))
            .unwrap();
        let rows: Vec<String> = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let marker_row = rows.iter().find(|row| row.contains("❯ google/model-a"));
        assert!(marker_row.is_some(), "selected row is marked: {rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("❯ google/model-b")),
            "only the selected row is marked: {rows:?}"
        );
    }

    /// A selection scrolled to the bottom of a long list must land on the
    /// last model row, never on the footer row underneath the list.
    #[test]
    fn the_marker_never_lands_on_the_footer_row() {
        let models: Vec<GeminiModel> = (0..20).map(|i| model(&format!("model-{i:02}"))).collect();
        let mut picker = PickerScreen::new(models, PickerPurpose::Initial, "google");
        picker.selected = 19;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker, &DARK))
            .unwrap();
        let rows: Vec<String> = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("❯ google/model-19")),
            "the bottom selection is marked: {rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("filter:") && row.contains("❯")),
            "the footer row never carries the marker: {rows:?}"
        );
    }

    /// A word wider than the row is hard-split: no row exceeds the width
    /// and the pieces stay in order.
    #[test]
    fn wrap_hard_splits_oversized_words() {
        let word = "x".repeat(100);
        let line = Line::from(Span::raw(word.clone()));
        let rows = wrap_line(&line, 30);
        assert!(rows.len() >= 4, "100 chars at width 30 need 4 rows");
        let joined: String = rows
            .iter()
            .flat_map(|row| row.iter().map(|span| span.content.clone()))
            .collect();
        assert_eq!(joined, word);
        for row in &rows {
            let row_width: usize = row.iter().map(|span| span.content.width()).sum();
            assert!(row_width <= 30, "row exceeds width: {row:?}");
        }
    }

    /// A sentence wraps at word boundaries, in order, each row within the
    /// width.
    #[test]
    fn wrap_breaks_at_word_boundaries_in_order() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa".to_owned();
        let line = Line::from(Span::raw(text.clone()));
        let rows = wrap_line(&line, 20);
        let joined = rows
            .iter()
            .flat_map(|row| row.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        let expected = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            joined.split_whitespace().collect::<Vec<_>>().join(" "),
            expected
        );
        for row in &rows {
            let row_width: usize = row.iter().map(|span| span.content.width()).sum();
            assert!(row_width <= 20, "row exceeds width: {row:?}");
        }
        assert!(
            rows[0].iter().any(|span| span.content.contains("alpha")),
            "the first word stays on the first row"
        );
        assert!(
            rows.last()
                .unwrap()
                .iter()
                .any(|span| span.content.contains("kappa")),
            "the last word lands on the last row"
        );
    }
}

#[cfg(all(test, feature = "mock"))]
mod scroll_tests {
    use super::render;
    use crate::app::{App, AppSetup, RunOptions};
    use crate::args::ThemeKind;
    use crate::store::MemoryStore;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn chat_app() -> App {
        let config = rhema_config::Config {
            google_api_key: Some("k".into()),
            model: Some("m".into()),
            ..rhema_config::Config::default()
        };
        App::new(AppSetup {
            store: Arc::new(MemoryStore::default()),
            config,
            backend: Some(crate::app::Backend::Google {
                api_key: "k".into(),
            }),
            session: rhema_core::Session::default(),
            options: RunOptions {
                theme: ThemeKind::Dark,
                workspace: PathBuf::from("."),
                max_steps: 4,
                no_confirm: false,
                save: None,
            },
        })
        .unwrap()
    }

    fn screen_text(app: &App) -> String {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect()
    }

    #[test]
    fn long_lines_wrap_and_scroll_reaches_the_top() {
        let mut app = chat_app();
        let long = format!("{} end-marker", "word ".repeat(200));
        app.transcript.push_user(&long);

        // At the tail, the last word is visible.
        let text = screen_text(&app);
        assert!(text.contains("end-marker"), "{text}");

        // Scrolled far back, the first words are visible.
        app.chat.scroll = 1000;
        let text = screen_text(&app);
        assert!(text.contains("word word word"), "{text}");
        assert!(!text.contains("end-marker"), "{text}");
    }
}
