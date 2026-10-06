//! Rendering only: maps the app state onto ratatui widgets. No logic —
//! every decision lives in [`crate::app`] and [`crate::screens`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
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

    // The list and the footer must not share a row: a scrolled-to-bottom
    // list draws its highlight on the last inner row, exactly where the
    // footer paints.
    let [list_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

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
    // Without an explicit highlight the selection is invisible: ListState
    // only offsets the scroll, the marker and style are opt-in.
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(Color::DarkGray)),
        list_area,
        &mut state,
    );

    let footer = Line::from(vec![
        Span::raw("filter: "),
        Span::raw(picker.filter.clone()).bold(),
        Span::raw("▌").bold(),
        Span::raw("  "),
        Span::raw(format!("{} models", filtered.len())).dim(),
        Span::raw("  "),
        Span::raw("type to filter · enter choose · esc clear/quit").dim(),
    ]);
    frame.render_widget(Paragraph::new(footer), footer_area);
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

#[cfg(test)]
mod tests {
    use super::*;
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
        );
        assert_eq!(picker.selected, 0);

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker))
            .unwrap();
        let rows: Vec<String> = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let marker_row = rows.iter().find(|row| row.contains("❯ model-a"));
        assert!(marker_row.is_some(), "selected row is marked: {rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("❯ model-b")),
            "only the selected row is marked: {rows:?}"
        );
    }

    /// A selection scrolled to the bottom of a long list must land on the
    /// last model row, never on the footer row underneath the list.
    #[test]
    fn the_marker_never_lands_on_the_footer_row() {
        let models: Vec<GeminiModel> = (0..20).map(|i| model(&format!("model-{i:02}"))).collect();
        let mut picker = PickerScreen::new(models, PickerPurpose::Initial);
        picker.selected = 19;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker))
            .unwrap();
        let rows: Vec<String> = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|row| row.contains("❯ model-19")),
            "the bottom selection is marked: {rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("filter:") && row.contains("❯")),
            "the footer row never carries the marker: {rows:?}"
        );
    }
}
