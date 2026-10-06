//! Rendering only: maps the app state onto ratatui widgets. No logic —
//! every decision lives in [`crate::app`] and [`crate::screens`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph};

use crate::app::{App, Screen};
use crate::screens::{COMING_SOON, PROVIDERS, PickerScreen, PickerStatus};
use crate::transcript::{self, pretty_args, truncate_chars};

/// Renders one frame.
pub fn render(app: &App, frame: &mut Frame) {
    match &app.screen {
        Screen::Login(login) => render_login(frame, login),
        Screen::Picker(picker) => render_picker(frame, picker),
        Screen::Chat => {
            let [transcript_area, status_area, input_area] = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .areas(frame.area());
            render_transcript(frame, app, transcript_area);
            render_status(frame, app, status_area);
            render_input(frame, app, input_area);
            if let Some(call) = &app.pending_approval {
                render_approval_modal(frame, call);
            } else if app.confirm_quit {
                render_confirm_quit(frame);
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

fn render_login(frame: &mut Frame, login: &crate::screens::LoginScreen) {
    let area = centered(58, 13, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
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
                spans.push(Span::raw(entry.name).dim());
                spans.push(Span::raw("  "));
                spans.push(Span::raw(COMING_SOON).dim());
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(login.selected));
    frame.render_stateful_widget(List::new(rows), providers_area, &mut state);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("API key: "),
            Span::raw(login.masked_input()).bold(),
        ])),
        input_area,
    );

    let status = match &login.status {
        crate::screens::LoginStatus::Idle => {
            Line::from("enter your Google API key, then press enter").dim()
        }
        crate::screens::LoginStatus::Validating => Line::from("validating key…").dim(),
        crate::screens::LoginStatus::Error(message) => Line::from(message.clone()).red(),
    };
    frame.render_widget(Paragraph::new(status), status_area);
}

fn render_picker(frame: &mut Frame, picker: &PickerScreen) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" select a model ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if picker.status != PickerStatus::Ready {
        let message = match &picker.status {
            PickerStatus::Loading => Line::from("loading models…").dim(),
            // The body below renders only in the Ready state.
            PickerStatus::Ready => Line::from(""),
            PickerStatus::Error(message) => Line::from(vec![
                Span::raw(message.clone()).red(),
                Span::raw("  "),
                Span::raw("press r to retry").dim(),
            ]),
        };
        frame.render_widget(
            Paragraph::new(message).block(Block::default().padding(Padding::new(1, 1, 1, 0))),
            inner,
        );
        return;
    }

    let filtered = picker.filtered();
    let rows: Vec<ListItem> = filtered
        .iter()
        .map(|model| {
            ListItem::new(Line::from(vec![
                Span::raw(model.id.clone()).bold(),
                Span::raw("  "),
                Span::raw(model.display_name.clone()),
                Span::raw("  "),
                Span::raw(format!(
                    "in {} / out {}",
                    model
                        .input_token_limit
                        .map(|limit| limit.to_string())
                        .unwrap_or_else(|| "?".into()),
                    model
                        .output_token_limit
                        .map(|limit| limit.to_string())
                        .unwrap_or_else(|| "?".into())
                ))
                .dim(),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    frame.render_stateful_widget(List::new(rows), inner, &mut state);

    let footer = if picker.filtering {
        Line::from(vec![
            Span::raw("filter: "),
            Span::raw(picker.filter.clone()).bold(),
            Span::raw("▌").bold(),
            Span::raw("  "),
            Span::raw("enter picks, esc clears").dim(),
        ])
    } else {
        Line::from(vec![
            Span::raw(format!("{} models", filtered.len())).dim(),
            Span::raw("  "),
            Span::raw("/ filter · enter choose · esc quit").dim(),
        ])
    };
    frame.render_widget(
        Paragraph::new(footer),
        Rect {
            x: area.x + 1,
            y: area.bottom().saturating_sub(2),
            width: area.width.saturating_sub(2),
            height: 1,
        },
    );
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" rhema ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = app.transcript.lines().iter().map(render_line).collect();
    let visible = inner.height as usize;
    let max_top = lines.len().saturating_sub(visible);
    let top = max_top.saturating_sub(app.chat.scroll.min(max_top));
    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
}

fn render_line(line: &transcript::Line) -> Line<'static> {
    match line {
        transcript::Line::User(text) => Line::from(vec![
            Span::raw("❯ ").green().bold(),
            Span::raw(text.clone()),
        ]),
        transcript::Line::Assistant(text) => Line::from(Span::raw(text.clone())),
        transcript::Line::ToolCall { name, args } => Line::from(
            Span::raw(format!(
                "⏺ {}({})",
                name,
                truncate_chars(args, transcript::ARGS_WIDTH)
            ))
            .cyan(),
        ),
        transcript::Line::ToolResult { ok, summary } => {
            let mark = if *ok { "✓" } else { "✗" };
            let line = Line::from(Span::raw(format!("  {mark} {summary}")));
            if *ok { line.green() } else { line.red() }
        }
        transcript::Line::Notice(text) => Line::from(Span::raw(text.clone()).dim().italic()),
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(error) = &app.error {
        Line::from(error.clone()).red().bold()
    } else if app.running {
        Line::from("thinking…").dim().italic()
    } else {
        match app.usage {
            Some(usage) => Line::from(format!(
                "tokens: {} in / {} out ({} cached)",
                usage.input_tokens, usage.output_tokens, usage.cached_input_tokens
            ))
            .dim(),
            None => Line::from("type a prompt · enter sends · ctrl-p model · esc quit").dim(),
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.running {
        " input (thinking…) "
    } else {
        " input "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
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
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(prompt).green(),
            Span::raw(visible),
        ])),
        inner,
    );
    if !app.running {
        frame.set_cursor_position(Position {
            x: inner.x + prompt.chars().count() as u16 + (app.chat.cursor - start) as u16,
            y: inner.y,
        });
    }
}

fn render_approval_modal(frame: &mut Frame, call: &rhema_core::ToolCall) {
    let area = centered(64, 12, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" approve tool call ")
        .yellow();
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
        Paragraph::new(Line::from("y allow · anything else declines").dim()),
        footer_area,
    );
}

fn render_confirm_quit(frame: &mut Frame) {
    let area = centered(34, 3, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
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
