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
use crate::screens::{PROVIDERS, PickerScreen, PickerStatus, SettingsSection, format_tokens};
use crate::transcript::{self, pretty_args};
use rhema_provider::ModelCard;
use std::path::Path;

/// One renderable unit of the transcript, the granularity the wrap
/// cache works at: one markdown response, one prompt, a tight run of
/// tool calls, a notice, or a system markdown block. Partitioning is
/// prefix-stable — appending lines extends the last block or adds one,
/// never rejoins an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
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
struct TranscriptBlock {
    start: usize,
    end: usize,
    kind: Kind,
}

/// Partitions the transcript lines into blocks. A whitespace-only
/// assistant line with no response behind it is noise — models emit
/// empty text parts between tool calls — and belongs to no block.
fn partition(lines: &[transcript::Line]) -> Vec<TranscriptBlock> {
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
            transcript::Line::User(_) => Kind::User,
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
    first: transcript::Line,
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
    match &app.screen {
        Screen::Picker(picker) => render_picker(frame, picker, &theme),
        Screen::Profile(profile) => render_profile_picker(frame, profile, &theme),
        Screen::Sandbox(sandbox) => render_sandbox_picker(frame, sandbox, &theme),
        Screen::Resume(resume) => render_resume_picker(frame, resume, &theme),
        Screen::Settings(settings) => render_settings(frame, settings, &theme, icons, &mut hits),
        Screen::Chat => {
            // An error wraps, so its strip grows to the wrapped height.
            // Either strip — error or spinner — carries a blank row above
            // it, so it never sits flush against the transcript's tail;
            // idle, the strip is just that single row of air between
            // transcript and input.
            let status_rows = match &app.error {
                Some(error) => {
                    wrap_line(&Line::from(error.clone()), frame.area().width as usize).len() as u16
                        + 1
                }
                None if app.turn.running => {
                    2 + u16::from(app.turn.subagent.is_some())
                        + thinking_rows(app, frame.area().width as usize)
                }
                None => 1,
            };
            let input_rows = input_rows(
                &app.chat.input,
                &app.chat.attachments,
                frame.area().width,
                &theme,
            );
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

/// The add-instance dialog: a pop-in box over the Providers page with
/// the provider list, the name and credential inputs, or the running
/// flow. The provider rows register click targets.
fn render_add_dialog(
    frame: &mut Frame,
    dialog: &crate::screens::AddInstanceDialog,
    theme: &Theme,
    hits: &mut Vec<(Rect, SettingsHit)>,
) {
    // The inner width of the centered box: the text lines wrap to it.
    const INNER_WIDTH: usize = 56;
    let flow = dialog_flow_lines(dialog, theme, INNER_WIDTH);
    let rows = match dialog.stage {
        crate::screens::DialogStage::SelectProvider => PROVIDERS.len(),
        crate::screens::DialogStage::NameInstance
        | crate::screens::DialogStage::EnterCredential => 1,
        crate::screens::DialogStage::Flow => flow.len().max(1),
    } as u16;
    let height = 2 + rows + 1 + 1;
    let area = centered(58, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" add a login ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, _input_area, status_area] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    match dialog.stage {
        crate::screens::DialogStage::SelectProvider => {
            // The provider kinds a new instance can be, each row a click
            // target; enter (or a second click's enter) confirms.
            let rows: Vec<ListItem> = PROVIDERS
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let selected = index == dialog.selected;
                    hits.push((
                        Rect {
                            x: body_area.x,
                            y: body_area.y + index as u16,
                            width: body_area.width,
                            height: 1,
                        },
                        SettingsHit::DialogProvider(index),
                    ));
                    let name_style = if selected {
                        Style::default().fg(theme.text).bold()
                    } else {
                        Style::default().fg(theme.text2)
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                        Span::styled(entry.name.to_string(), name_style),
                    ]))
                })
                .collect();
            frame.render_widget(ratatui::widgets::List::new(rows), body_area);
        }
        crate::screens::DialogStage::NameInstance
        | crate::screens::DialogStage::EnterCredential => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(dialog.input_label()).fg(theme.text3),
                    Span::raw(dialog.masked_input()).bold(),
                ])),
                body_area,
            );
        }
        crate::screens::DialogStage::Flow => {
            frame.render_widget(Paragraph::new(flow), body_area);
        }
    }

    let status = match (&dialog.stage, &dialog.status) {
        // The flow stage renders its own progress; the status line stays
        // the way out.
        (crate::screens::DialogStage::Flow, crate::screens::DialogStatus::Error(message)) => {
            Line::from(message.clone()).fg(theme.error)
        }
        (crate::screens::DialogStage::Flow, _) => Line::from(dialog.input_hint()).fg(theme.text3),
        (_, crate::screens::DialogStatus::Idle) => Line::from(dialog.input_hint()).fg(theme.text3),
        (_, crate::screens::DialogStatus::Validating) => Line::from("validating…").fg(theme.text3),
        (_, crate::screens::DialogStatus::Error(message)) => {
            Line::from(message.clone()).fg(theme.error)
        }
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
        .map(|entry| ListItem::new(model_row(entry.provider, &entry.model, theme)))
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

    let mut footer = vec![
        Span::raw("filter: "),
        Span::raw(picker.filter.clone()).bold(),
        Span::raw("▌").bold(),
        Span::raw("  "),
        Span::raw(format!("{} models", filtered.len())).fg(theme.text3),
    ];
    // A provider whose fetch failed while others answered: its absence
    // from the list must read as an error, not as an empty catalog.
    if !picker.failed.is_empty() {
        footer.push(
            Span::raw(format!("  · no models from {}", picker.failed.join(", "))).fg(theme.warn),
        );
    }
    footer.push(Span::raw("  "));
    footer.push(Span::raw("type to filter · enter choose · esc clear/quit").fg(theme.text3));
    frame.render_widget(Paragraph::new(Line::from(footer)), footer_area);
}

/// The profile picker: same frame as the model picker, rows in the
/// profile's own color, bold selected, the session's active profile
/// marked.
fn render_profile_picker(frame: &mut Frame, screen: &crate::screens::ProfileScreen, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" select a profile ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows: Vec<ListItem> = screen
        .entries
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let color = row
                .color
                .as_deref()
                .and_then(crate::palette::parse_hex_color)
                .unwrap_or_else(|| crate::palette::default_profile_color(&row.name, theme));
            let selected = index == screen.selected;
            let name_style = if selected {
                Style::default().fg(color).bold()
            } else {
                Style::default().fg(color)
            };
            let mut spans = vec![Span::styled(row.name.clone(), name_style)];
            let mut detail = String::new();
            if let Some(provider) = &row.provider {
                detail.push_str(provider);
            } else {
                detail.push_str("no provider");
            }
            if let Some(model) = &row.model {
                detail.push_str(&format!(" · {model}"));
            }
            if !row.usable {
                detail.push_str(" · no API key");
            }
            if row.active {
                detail.push_str(" · active");
            }
            spans.push(Span::raw("  ").fg(theme.text3));
            spans.push(Span::styled(detail, Style::default().fg(theme.text3)));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(screen.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(theme.elevated)),
        inner,
        &mut state,
    );

    let footer = Line::from(Span::raw("enter switch · esc back").fg(theme.text3));
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().padding(Padding::horizontal(1))),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

/// The sandbox picker: the profile picker's frame and keys over the
/// offered kinds — rounded border, `❯ ` highlight, one footer line.
fn render_sandbox_picker(frame: &mut Frame, screen: &crate::screens::SandboxScreen, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" where bash runs ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows: Vec<ListItem> = screen
        .entries
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let selected = index == screen.selected;
            let name_style = if selected {
                Style::default().fg(theme.text).bold()
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![Span::styled(row.kind.stored_name().to_owned(), name_style)];
            let mut detail = String::new();
            detail.push_str(match row.kind {
                crate::args::SandboxChoice::Local => "this host",
                crate::args::SandboxChoice::Docker => "a container over the docker CLI",
                crate::args::SandboxChoice::Ssh => "a remote box over ssh",
                crate::args::SandboxChoice::Http => "a rhema-sandboxd daemon",
            });
            if row.remembered {
                detail.push_str(" · remembered here");
            }
            if row.active {
                detail.push_str(" · active");
            }
            spans.push(Span::raw("  ").fg(theme.text3));
            spans.push(Span::styled(detail, Style::default().fg(theme.text3)));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(screen.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(theme.elevated)),
        inner,
        &mut state,
    );

    let footer = Line::from(Span::raw(
        "enter switch · esc back · edit config.yaml for url/host/token",
    ))
    .fg(theme.text3);
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().padding(Padding::horizontal(1))),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

fn render_resume_picker(frame: &mut Frame, screen: &crate::screens::ResumeScreen, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" resume a session ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows: Vec<ListItem> = screen
        .entries
        .iter()
        .map(|row| {
            let snippet = row
                .snippet
                .clone()
                .unwrap_or_else(|| "(no preview)".to_owned());
            ListItem::new(Line::from(vec![
                Span::styled(snippet, Style::default().fg(theme.text)),
                Span::styled(
                    format!("  ·  {}", row.age),
                    Style::default().fg(theme.text3),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(screen.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(theme.elevated)),
        inner,
        &mut state,
    );

    let footer = Line::from(Span::raw("enter resume · esc back").fg(theme.text3));
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().padding(Padding::horizontal(1))),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

/// The settings screen: the sidebar of sections on the left, the selected
/// section's page on the right, its key hints pinned to the bottom row.
/// The settings screen: the sidebar of sections on the left, the
/// selected section's page — titled, ruled, padded — on the right, its
/// key hints pinned to the bottom row, and the add-instance dialog
/// popped over it all. Sidebar rows and interactive page rows register
/// click targets — the rects are what a click is matched against, so
/// only what was drawn is clickable.
fn render_settings(
    frame: &mut Frame,
    settings: &crate::screens::SettingsScreen,
    theme: &Theme,
    icons: &crate::icons::Icons,
    hits: &mut Vec<(Rect, SettingsHit)>,
) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [sidebar_area, page_area] =
        Layout::horizontal([Constraint::Length(16), Constraint::Min(24)]).areas(inner);
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(page_area);
    // The content pane wraps at its true text width: two cells of
    // padding on each side.
    let text_width = content_area.width.saturating_sub(4) as usize;

    // The sidebar: the gear heading, a rule, then one row per section,
    // the selected one marked and lit. Every row is a click target; the
    // rows sit under the top padding, the heading, and the rule.
    let rule = "─".repeat(sidebar_area.width.saturating_sub(2) as usize);
    let mut sidebar = vec![
        Line::from(vec![
            Span::raw(icons.settings).fg(theme.accent_text),
            Span::raw(" Settings").bold(),
        ]),
        Line::from(Span::raw(rule).fg(theme.border_strong)),
    ];
    for section in SettingsSection::ALL {
        let selected = *section == settings.section;
        let style = if selected {
            Style::default().fg(theme.accent_text).bold()
        } else {
            Style::default().fg(theme.text2)
        };
        sidebar.push(Line::from(vec![
            Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
            Span::styled(section.label().to_owned(), style),
        ]));
    }
    for (index, section) in SettingsSection::ALL.iter().enumerate() {
        hits.push((
            Rect {
                x: sidebar_area.x + 1,
                y: sidebar_area.y + 3 + index as u16,
                width: sidebar_area.width.saturating_sub(2),
                height: 1,
            },
            SettingsHit::Sidebar(*section),
        ));
    }
    frame.render_widget(
        Paragraph::new(sidebar).block(Block::default().padding(Padding::new(1, 1, 1, 0))),
        sidebar_area,
    );

    let (page, footer, row_hits) = settings_page(settings, theme, text_width);
    frame.render_widget(
        Paragraph::new(page).block(Block::default().padding(Padding::new(2, 2, 1, 0))),
        content_area,
    );
    // The interactive rows sit at their rendered line indices — every
    // line the page builder produced is already wrapped to the pane's
    // text width, and the top padding offsets them one row down.
    for (line_index, hit) in row_hits {
        hits.push((
            Rect {
                x: content_area.x + 2,
                y: content_area.y + 1 + line_index as u16,
                width: text_width as u16,
                height: 1,
            },
            hit,
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::default().padding(Padding::horizontal(2))),
        footer_area,
    );

    if let Some(dialog) = &settings.dialog {
        render_add_dialog(frame, dialog, theme, hits);
    }
}

/// Tokens as the Usage page spells an amount: kilo-scaled once past
/// a thousand, one decimal, the trailing `.0` trimmed.
fn usage_k(tokens: u64) -> String {
    if tokens >= 1_000 {
        let k = format!("{:.1}k", tokens as f64 / 1_000.0);
        match k.strip_suffix(".0k") {
            Some(base) => format!("{base}k"),
            None => k,
        }
    } else {
        tokens.to_string()
    }
}

/// One `label  value` row of a settings page; the label column reads
/// dim. Wrapped to the pane, so the rendered line count is the
/// builder's to know.
fn settings_field(
    label: &str,
    value: Span<'static>,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    wrap_line(
        &Line::from(vec![
            Span::raw(format!("{:<18}", label)).fg(theme.text3),
            value,
        ]),
        width,
    )
}

/// A secondary note, wrapped to the pane.
fn settings_hint(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    wrap_line(
        &Line::from(Span::raw(text.to_owned()).fg(theme.text3)),
        width,
    )
}

/// A selectable page row: the ❯ marker, the line lit when selected.
fn settings_row(line: Line<'static>, selected: bool, theme: &Theme) -> Line<'static> {
    if selected {
        line.style(Style::default().bg(theme.elevated))
    } else {
        line
    }
}

/// A settings page: its content lines, its footer hints, and the click
/// targets of its interactive rows as rendered line indices — the text
/// is wrapped to the pane's width, so the indices are exact.
fn settings_page(
    settings: &crate::screens::SettingsScreen,
    theme: &Theme,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<Span<'static>>,
    Vec<(usize, SettingsHit)>,
) {
    let footer = |hints: &str| vec![Span::raw(hints.to_owned()).fg(theme.text3)];
    // Every page opens with its title and a rule, mirroring the
    // sidebar's heading, then one row of air before the content. The
    // interactive row indices count these header lines in.
    let mut lines = vec![
        Line::from(Span::raw(settings.section.label()).bold()),
        Line::from(Span::raw("─".repeat(width)).fg(theme.border_strong)),
        Line::default(),
    ];
    // The startup gate's explanation rides over every page: the
    // settings opened because no backend resolved, and the reason
    // must not vanish when the section changes.
    if let Some(reason) = &settings.startup_reason {
        lines.extend(wrap_line(
            &Line::from(Span::raw(reason.clone()).fg(theme.warn)),
            width,
        ));
        lines.push(Line::default());
    }
    match settings.section {
        SettingsSection::Providers => {
            let provider_value = match settings.stored.provider_name() {
                None => Span::raw("None".to_owned()).fg(theme.warn),
                Some(provider) => match &settings.stored.instance {
                    Some(instance) => Span::raw(format!("{instance} ({provider})")).fg(theme.text),
                    None => Span::raw(provider).fg(theme.text),
                },
            };
            let model_value = match &settings.stored.model {
                Some(model) => Span::raw(model.clone()).fg(theme.text),
                None => Span::raw("None".to_owned()).fg(theme.text3),
            };
            lines.extend(settings_field(
                "Profile",
                Span::raw(settings.profile.clone()).fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field("Provider", provider_value, theme, width));
            lines.extend(settings_field("Model", model_value, theme, width));
            lines.push(Line::default());
            lines.push(Line::from(Span::raw("Logins").bold()));
            // One interactive row per instance plus the add row.
            let mut row_hits = Vec::new();
            for (index, row) in settings.instance_rows().into_iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                let marker = Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text);
                let line =
                    if row.add {
                        Line::from(vec![
                            marker,
                            Span::raw("+ Add a new instance...").fg(theme.text2),
                        ])
                    } else {
                        Line::from(vec![
                            marker,
                            Span::raw("● ").fg(if row.active { theme.ok } else { theme.text3 }),
                            Span::raw(format!("{} ({})", row.name, row.provider))
                                .fg(if row.active { theme.text } else { theme.text2 }),
                            if row.active {
                                Span::raw(" active").fg(theme.text3)
                            } else {
                                Span::raw("").fg(theme.text)
                            },
                        ])
                    };
                lines.push(settings_row(line, selected, theme));
            }
            (lines, footer("d - delete provider"), row_hits)
        }
        SettingsSection::Appearance => {
            let mut row_hits = Vec::new();
            let toggles = [
                ("Colors", settings.colors.label()),
                ("Theme", if settings.theme_dark { "Dark" } else { "Light" }),
                (
                    "Icons",
                    if settings.icons_nerd {
                        "Nerd"
                    } else {
                        "Unicode"
                    },
                ),
            ];
            for (index, (label, value)) in toggles.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                let line = Line::from(vec![
                    Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                    Span::raw(format!("{:<10}", label)).fg(theme.text3),
                    Span::raw((*value).to_owned()).fg(theme.text),
                ]);
                lines.push(settings_row(line, selected, theme));
            }
            lines.push(Line::default());
            (lines, footer("c - colors, t - theme, i - icons"), row_hits)
        }
        SettingsSection::Memory => {
            lines.extend(wrap_line(
                &Line::from(
                    Span::raw(
                        "Cross-session memory is not wired into this coding agent yet.".to_owned(),
                    )
                    .fg(theme.text),
                ),
                width,
            ));
            lines.push(Line::default());
            (lines, footer(""), Vec::new())
        }
        SettingsSection::Skills => {
            lines.extend(settings_field(
                "Global",
                Span::raw(
                    settings
                        .skills_global_dir
                        .clone()
                        .unwrap_or_else(|| "None".into()),
                )
                .fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field(
                "Workspace",
                Span::raw(settings.skills_workspace_dir.clone()).fg(theme.text),
                theme,
                width,
            ));
            lines.push(Line::default());
            if settings.skills.is_empty() {
                lines.extend(settings_hint("No skills discovered", theme, width));
            }
            for skill in &settings.skills {
                lines.extend(wrap_line(
                    &Line::from(vec![
                        Span::raw(format!("{} - ", skill.name)).fg(theme.text),
                        Span::raw(skill.description.clone()).fg(theme.text3),
                    ]),
                    width,
                ));
            }
            (lines, footer(""), Vec::new())
        }
        SettingsSection::Usage => {
            let allowance_value: Vec<Span<'static>> = match &settings.allowance {
                Some((backend, Some(allowance))) => {
                    let mut spans = vec![
                        Span::raw(format!("{backend} · ")).fg(theme.text),
                        Span::raw(allowance.amount.clone()).fg(theme.text),
                    ];
                    if !allowance.kind.is_empty() {
                        spans.push(Span::raw(format!(" {}", allowance.kind)).fg(theme.text3));
                    }
                    spans
                }
                Some((backend, None)) => {
                    vec![Span::raw(format!("{backend} · nothing reported")).fg(theme.text3)]
                }
                None => {
                    vec![Span::raw("no allowance from this backend".to_owned()).fg(theme.text3)]
                }
            };
            let cost = if settings.session_cost >= 0.01 || settings.session_cost == 0.0 {
                format!("${:.2}", settings.session_cost)
            } else {
                format!("${:.4}", settings.session_cost)
            };
            let last_turn = match settings.last_turn {
                Some(usage) => Span::raw(format!(
                    "{} in · {} cached · {} out",
                    format_tokens(Some(u64::from(usage.input_tokens))),
                    format_tokens(Some(u64::from(usage.cached_input_tokens))),
                    format_tokens(Some(u64::from(usage.output_tokens))),
                ))
                .fg(theme.text),
                None => Span::raw("no turn finished yet".to_owned()).fg(theme.text3),
            };
            lines.extend(wrap_line(
                &Line::from(
                    std::iter::once(Span::raw(format!("{:<12}", "allowance")).fg(theme.text3))
                        .chain(allowance_value)
                        .collect::<Vec<_>>(),
                ),
                width,
            ));
            lines.extend(settings_field(
                "session",
                Span::raw(cost).fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field("last turn", last_turn, theme, width));
            // The compaction benchmark, when the transcript is on: the
            // cache rate is the providers' own accounting, the rest is
            // what the stepped view did this session.
            if let Some(stats) = settings.transcript {
                let cache = match stats.cache_hit_rate() {
                    Some(rate) => format!(
                        "{:.0}% · {} of {} tokens",
                        rate * 100.0,
                        format_tokens(Some(stats.cached_input_tokens)),
                        format_tokens(Some(stats.input_tokens)),
                    ),
                    None => "no usage yet".to_owned(),
                };
                lines.extend(settings_field(
                    "cache",
                    Span::raw(cache).fg(theme.text),
                    theme,
                    width,
                ));
                let worker = if stats.summarizer_calls > 0 {
                    format!(
                        "worker {} calls, {} failed",
                        stats.summarizer_calls, stats.summarizer_failures
                    )
                } else {
                    "extractive".to_owned()
                };
                let compaction = format!(
                    "{}/{} requests viewed, {} flushes, block {}, {}",
                    stats.viewed,
                    stats.requests,
                    stats.flushes,
                    format_tokens(Some(stats.block_bytes as u64)),
                    worker,
                );
                lines.extend(settings_field(
                    "compaction",
                    Span::raw(compaction).fg(theme.text),
                    theme,
                    width,
                ));
            }
            // The detailed split: what the session's tokens bought,
            // each as its share of the total and its own amount. The
            // numbers ride the transcript's breakdown, so a
            // transcript-less session has nothing to show.
            lines.push(Line::default());
            lines.push(Line::from(Span::raw("Detailed session usage")).bold());
            match settings.transcript {
                Some(stats) => {
                    let total = stats.usage.total();
                    for (label, tokens) in stats.usage.labeled() {
                        let pct = if total > 0 {
                            tokens as f64 / total as f64 * 100.0
                        } else {
                            0.0
                        };
                        lines.extend(settings_field(
                            label,
                            Span::raw(format!("{:.0}% ({})", pct, usage_k(tokens))).fg(theme.text),
                            theme,
                            width,
                        ));
                    }
                }
                None => {
                    lines.extend(settings_hint("no transcript in this session", theme, width));
                }
            }
            (lines, footer("esc back"), Vec::new())
        }
        SettingsSection::About => {
            lines.push(Line::from(
                Span::raw(format!("Rhema Code {}", env!("CARGO_PKG_VERSION"))).bold(),
            ));
            lines.extend(settings_field(
                "Repository",
                Span::raw(env!("CARGO_PKG_REPOSITORY").to_owned()).fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field(
                "Config",
                Span::raw(settings.config_path.clone().unwrap_or_else(|| "-".into()))
                    .fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field(
                "Workspace",
                Span::raw(settings.workspace.clone()).fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field(
                "Profiles",
                Span::raw(settings.profile_count.to_string()).fg(theme.text),
                theme,
                width,
            ));
            (lines, footer(""), Vec::new())
        }
    }
}
fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
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
fn render_block(
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
                transcript::Line::User(text) => Some(text.as_str()),
                _ => None,
            })
            .flat_map(|text| render_user(text, theme))
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
fn render_user(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut pastes = 0usize;
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
                    .extend(chip_spans(payload, pastes, theme));
                pastes += 1;
            }
        }
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

/// One picker row: `provider/` dim, id bright, then display name, price,
/// and context window dim.
fn model_row(provider: &str, model: &ModelCard, theme: &Theme) -> Line<'static> {
    let mut details = String::new();
    if let Some(pricing) = model.pricing {
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

/// The model id as the footer shows it: everything after the last `/`,
/// so an OpenRouter id like `apodex/apodex-1.1-mini:free` reads without
/// its vendor prefix. Ids without a slash pass through whole.
fn footer_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Parents the footer's folder may show beside the folder itself —
/// three folders in all.
const FOOTER_FOLDER_PARENTS: usize = 2;

/// The workspace as the footer shows it: `(prefix, parents, folder)`.
/// The home prefix is trimmed away — and the home's `Documents` with
/// it, where projects live, the word carries nothing — then at most
/// [`FOOTER_FOLDER_PARENTS`] parents ride beside the folder itself so a
/// deep path stays small. `prefix` is `/` for a whole absolute path,
/// `…/` when parents fell off. A path with nothing left after the trim
/// (the home itself, its `Documents`) falls back to its own name.
fn footer_folder(workspace: &Path, home: Option<&Path>) -> (String, Vec<String>, String) {
    let mut rel = workspace.to_path_buf();
    if let Some(home) = home {
        if let Ok(stripped) = rel.strip_prefix(home) {
            rel = stripped.to_path_buf();
        }
        if let Ok(stripped) = rel.strip_prefix("Documents") {
            rel = stripped.to_path_buf();
        }
    }
    let text = rel.to_string_lossy();
    let absolute = text.starts_with('/');
    let names: Vec<&str> = text.split('/').filter(|name| !name.is_empty()).collect();
    let Some(folder) = names.last() else {
        let fallback = workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_owned());
        return (String::new(), Vec::new(), fallback);
    };
    let truncated = names.len() > FOOTER_FOLDER_PARENTS + 1;
    let keep = names.len().min(FOOTER_FOLDER_PARENTS + 1);
    let parents: Vec<String> = names[names.len() - keep..names.len() - 1]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let prefix = if truncated {
        "…/".to_owned()
    } else if absolute {
        "/".to_owned()
    } else {
        String::new()
    };
    (prefix, parents, (*folder).to_owned())
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

fn render_line(line: &transcript::Line, theme: &Theme, robot: &'static str) -> Line<'static> {
    match line {
        transcript::Line::User(text) => {
            // Embedded pastes collapse to their chips here too — this
            // arm is the fallback off the transcript's multi-row path.
            let mut spans = vec![Span::raw("❯ ").fg(theme.text3)];
            let mut pastes = 0usize;
            for part in rhema_core::paste::parts(text) {
                match part {
                    rhema_core::paste::Part::Text(text) => {
                        spans.push(Span::raw(text.replace('\n', " ")));
                    }
                    rhema_core::paste::Part::Paste(payload) => {
                        spans.extend(chip_spans(payload, pastes, theme));
                        pastes += 1;
                    }
                }
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
fn name_is_bash(line: &transcript::Line) -> bool {
    matches!(line, transcript::Line::ToolCall { name, .. } if name == "bash")
}

/// One tool-call row, built at the terminal's width: `$ command` with
/// the exit code flush right. The command ellipsizes to what fits; a
/// call still running or rejected before a shell has no code to show.
fn tool_row(
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

fn render_status(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let width = area.width as usize;
    let mut lines: Vec<Line> = if let Some(error) = &app.error {
        // Wrapped a cell short so the indent below cannot push a wrap.
        wrap_line(&Line::from(error.clone()), width.saturating_sub(1))
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.error).bold()))
            .collect()
    } else if app.turn.running {
        // Braille dots cycling through the classic spinner, animated off
        // the running turn's clock — no timer machinery of its own.
        let spinner = spinner_frame(
            app.turn
                .started_at
                .map(|started| started.elapsed())
                .unwrap_or_default(),
        );
        // Two phases, two colors: the model streaming is thinking; a
        // command executing is running.
        let phase = if app.turn.processing {
            Line::from(vec![
                Span::styled(
                    SPINNER[spinner].to_string(),
                    Style::default().fg(theme.info),
                ),
                Span::raw(" running").fg(theme.info),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    SPINNER[spinner].to_string(),
                    Style::default().fg(theme.running),
                ),
                Span::raw(" thinking").fg(theme.running),
            ])
        };
        let mut line = phase;
        if let Some(started) = app.turn.started_at {
            line.spans
                .push(Span::raw(format!(" · {}", elapsed_text(started.elapsed()))).fg(theme.text3));
        }
        line.spans.push(Span::raw(" · esc cancels").fg(theme.text3));
        vec![line]
    } else {
        Vec::new()
    };
    // A working subagent pins its own line right below the spinner: the
    // robot badge, the animated delegate, its phase, spend, and age.
    if let Some(subagent) = &app.turn.subagent {
        lines.push(subagent_line(app, subagent, theme));
    }
    // The live preview rides under the spinner — the reasoning trace's
    // tail, or a running command's earned output — at most three dim
    // rows. The reasoning half is gone the moment real output starts,
    // because the buffer clears then.
    if app.turn.running && app.error.is_none() {
        lines.extend(thinking_preview(app, width, theme));
    }
    // Content sits one cell in, on the transcript's left inset, and a
    // blank row above the strip keeps it off the transcript's tail.
    for line in &mut lines {
        line.spans.insert(0, Span::raw(" "));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    } else {
        lines.insert(0, Line::from(""));
    }
    if app.chat.scroll > 0 && app.error.is_none() {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::raw("  ·  viewing history · pgdn: latest").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The classic braille spinner; frames step at 80ms off whichever clock
/// animates it.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The spinner frame for an elapsed time.
fn spinner_frame(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / 80) as usize % SPINNER.len()
}

/// An elapsed time as a clock reads it: seconds under the minute,
/// minutes and seconds under the hour, hours and minutes beyond —
/// `2055s` is unreadable where `34min 15s` reads at a glance.
fn elapsed_text(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}min {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}min", secs / 3600, (secs % 3600) / 60)
    }
}

/// A cost as the footer prints it: cents at a glance, four decimals
/// while the figure is still small enough for the fourth to matter.
fn cost_text(cost: f64) -> String {
    if cost >= 0.01 || cost == 0.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

/// The pinned live line of a working subagent: the robot badge and its
/// spinner, the delegate's name, the phase it last reported, the
/// context its model calls have spent against its model's window, the
/// cost that spend prices to, and its age. A figure the catalog cannot
/// price or size degrades to what is known — tokens alone, no cost.
fn subagent_line(app: &App, subagent: &crate::app::SubagentLive, theme: &Theme) -> Line<'static> {
    let spinner = spinner_frame(subagent.started_at.elapsed());
    let mut spans = vec![
        Span::raw(format!("{} ", app.icons.robot)).fg(theme.text3),
        Span::styled(
            SPINNER[spinner].to_string(),
            Style::default().fg(theme.info),
        ),
        Span::raw(" "),
        Span::raw(
            subagent
                .name
                .clone()
                .unwrap_or_else(|| "subagent".to_owned()),
        )
        .fg(theme.info),
        Span::raw(format!(" · {}", subagent.phase)).fg(theme.running),
    ];
    if let Some(usage) = subagent.usage {
        let mut figure = format_tokens(Some(crate::app::context_tokens(&usage)));
        let window = subagent
            .model
            .as_deref()
            .and_then(|model| app.model_window(model));
        if let Some(window) = window {
            figure.push_str(&format!("/{}", format_tokens(Some(window))));
        }
        spans.push(Span::raw(format!(" · {figure}")).fg(theme.text3));
        if let Some(pricing) = subagent
            .model
            .as_deref()
            .and_then(|model| app.model_pricing(model))
        {
            let cost = pricing.cost(
                u64::from(usage.input_tokens),
                u64::from(usage.output_tokens),
                u64::from(usage.cached_input_tokens),
            );
            spans.push(Span::raw(format!(" · {}", cost_text(cost))).fg(theme.text3));
        }
    }
    spans.push(
        Span::raw(format!(
            " · {}",
            elapsed_text(subagent.started_at.elapsed())
        ))
        .fg(theme.text3),
    );
    Line::from(spans)
}

/// Rows the live preview may show.
const THINKING_PREVIEW_ROWS: usize = 3;

/// A running command's output earns its preview after this long.
const TOOL_PREVIEW_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether the tool preview shows: a second of runtime or a second
/// completed line — an instant command never flashes its output.
fn tool_preview_ready(app: &App) -> bool {
    let Some(started) = app.turn.tool_output_at else {
        return false;
    };
    started.elapsed() >= TOOL_PREVIEW_DELAY || app.turn.tool_output.matches('\n').count() >= 2
}

/// The live preview's rows at `width`: a running command's output while
/// it has earned the screen, the reasoning trace's tail otherwise. Both
/// the strip's layout and its drawing come from here, so the reserve
/// matches what's on screen.
fn thinking_preview_rows(app: &App, width: usize) -> Vec<Line<'static>> {
    let text = if tool_preview_ready(app) {
        &app.turn.tool_output
    } else {
        &app.turn.thinking
    };
    let width = width.saturating_sub(1).max(1);
    let mut rows: Vec<Line<'static>> = completed_thinking(text)
        .flat_map(|line| wrap_line(&Line::from(Span::raw(line.to_owned())), width))
        .collect();
    let skip = rows.len().saturating_sub(THINKING_PREVIEW_ROWS);
    rows.drain(..skip);
    rows
}

/// The buffer's completed lines: everything it holds before the line
/// still streaming. The preview reveals a line at a time, never a word
/// at a time. Blank lines drop out — the preview has three rows to
/// spend and a blank one carries nothing.
fn completed_thinking(thinking: &str) -> impl Iterator<Item = &str> {
    let complete = match thinking.rfind('\n') {
        Some(end) => &thinking[..end],
        None => "",
    };
    complete.split('\n').filter(|line| !line.is_empty())
}

/// The live preview rows: the tail of the chosen buffer's completed
/// lines, wrapped and dim. Empty until a line completes.
fn thinking_preview(app: &App, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = Style::default().fg(theme.text3).italic();
    thinking_preview_rows(app, width)
        .into_iter()
        .map(|line| line.style(style))
        .collect()
}

/// How many rows the preview occupies at `width` — the same wrap the
/// render applies, so the strip reserves exactly what it draws.
fn thinking_rows(app: &App, width: usize) -> u16 {
    thinking_preview_rows(app, width).len() as u16
}

/// The dim telemetry strip under the input: last turn's tokens, and the
/// history-pinned indicator.
/// The completion popup: while the input names commands or the cursor
/// sits on a path, the matches float above it — full terminal width, the
/// shared background, no panel. The command reads in accent, the
/// description dim, and the highlighted row carries the elevated tone.
/// Tab completes, enter runs.
fn render_command_palette(frame: &mut Frame, app: &App, input_area: Rect, theme: &Theme) {
    let profiles = app.palette_profiles();
    let rows =
        crate::screens::palette_rows(&app.chat.input, app.chat.cursor, &app.models, &profiles);
    if rows.is_empty() {
        return;
    }
    let visible = rows.len().min(6) as u16;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(visible + 2),
        width: input_area.width,
        height: visible + 2,
    };
    // Clear erases the transcript beneath; the well tone is painted back
    // so the rows sit on the shared background, framed by a hairline.
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The window follows the selection: more rows than lines scroll the
    // list once the highlight passes the bottom, instead of hiding it.
    let count = rows.len();
    let visible = visible as usize;
    let first = if count > visible {
        (app.chat.palette_index + 1)
            .saturating_sub(visible)
            .clamp(0, count - visible)
    } else {
        0
    };
    let selected = app.chat.palette_index.min(count - 1);
    // Descriptions align in a column: every name is padded to the longest
    // visible name plus a three-space gap.
    let shown = &rows[first..first + visible];
    let name_width = shown
        .iter()
        .map(|row| palette_row_name(row).width())
        .max()
        .unwrap_or(0)
        + 3;
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .map(|(index, row)| {
            // The selection lives in the text, not the row: bold marks it,
            // the background stays shared.
            let selected_row = first + index == selected;
            let (name, description) = palette_row_texts(row);
            let mut spans = vec![Span::styled(
                format!("{:<name_width$}", name),
                if selected_row {
                    Style::default().fg(theme.accent_text).bold()
                } else {
                    Style::default().fg(theme.accent_text)
                },
            )];
            spans.push(Span::styled(description, Style::default().fg(theme.text3)));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The palette row's name column: the command's name, the model's
/// provider-prefixed id — the same shape the picker's rows carry — the
/// profile's name, or the path entry's name.
fn palette_row_name(row: &crate::screens::PaletteRow) -> String {
    match row {
        crate::screens::PaletteRow::Command(spec) => spec.name.to_owned(),
        crate::screens::PaletteRow::Model { provider, id, .. } => format!("{provider}/{id}"),
        crate::screens::PaletteRow::Profile { name, .. } => name.clone(),
        crate::screens::PaletteRow::Choice { value, .. } => (*value).to_owned(),
        crate::screens::PaletteRow::Path { name, .. } => name.clone(),
    }
}

/// The palette row's description: the command's own, the model's display
/// name with its facts, the profile's provider and model, or the path
/// entry's kind.
fn palette_row_texts(row: &crate::screens::PaletteRow) -> (String, String) {
    match row {
        crate::screens::PaletteRow::Command(spec) => {
            (palette_row_name(row), spec.description.to_string())
        }
        crate::screens::PaletteRow::Model { description, .. } => {
            (palette_row_name(row), description.clone())
        }
        crate::screens::PaletteRow::Profile { description, .. } => {
            (palette_row_name(row), description.clone())
        }
        crate::screens::PaletteRow::Choice { description, .. } => {
            (palette_row_name(row), (*description).to_owned())
        }
        crate::screens::PaletteRow::Path { is_dir, .. } => (
            palette_row_name(row),
            if *is_dir {
                "directory".to_owned()
            } else {
                String::new()
            },
        ),
    }
}

/// The thinking badge's letter: the glyph reads with one letter beside
/// it, L for low through H for high.
fn effort_letter(effort: rhema_core::request::ThinkingEffort) -> &'static str {
    use rhema_core::request::ThinkingEffort;
    match effort {
        ThinkingEffort::Default => "",
        ThinkingEffort::Low => "L",
        ThinkingEffort::Medium => "M",
        ThinkingEffort::High => "H",
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // The mark, then always the same facts: profile, where the session
    // sits, model, context footprint against the model's window, the
    // session's accumulated cost. Key hints live in /help, not here.
    // Every segment owns its leading separator — a trailing one on the
    // previous segment doubles as soon as an optional segment (folder,
    // allowance) sits between two others.
    let effective = app.effective();
    let mut parts = vec![Span::raw(" ῥ").fg(theme.accent_text)];
    if let Some((name, color)) = app.active_profile() {
        parts.push(Span::raw(" · ").fg(theme.text3));
        parts.push(Span::raw(name).fg(color));
    }
    // Where the session sits: the folder badge over the trimmed
    // workspace path, its folder in bold.
    let (prefix, parents, folder) = footer_folder(&app.options.workspace, app.home.as_deref());
    parts.push(Span::raw(format!(" · {} ", app.icons.folder)).fg(theme.text3));
    parts.push(Span::raw(prefix).fg(theme.text3));
    for parent in parents {
        parts.push(Span::raw(parent).fg(theme.text3));
        parts.push(Span::raw("/").fg(theme.text3));
    }
    parts.push(Span::raw(folder).fg(theme.text2).bold());
    match (&effective.provider, &effective.model) {
        (None, _) => parts.push(Span::raw(" · no provider; use /login").fg(theme.text3)),
        (_, None) => parts.push(Span::raw(" · no model; use /model").fg(theme.text3)),
        // The id's vendor prefix is picker detail: a footer reads the
        // bare model name.
        (_, Some(model)) => {
            parts.push(Span::raw(" · ").fg(theme.text3));
            parts.push(Span::raw(footer_model(model).to_owned()).fg(theme.text2));
        }
    }
    // A named thinking effort is session-relevant telemetry: the badge
    // only appears when the level would change the request. The letter
    // rides the glyph; the word it abbreviates is the /thinking
    // argument, one keystroke away in the palette.
    if app.options.thinking != rhema_core::request::ThinkingEffort::Default {
        parts.push(
            Span::raw(format!(
                " · {} {}",
                app.icons.thinking,
                effort_letter(app.options.thinking)
            ))
            .fg(theme.text3),
        );
    }
    let used = app.context_used();
    let limit = app.current_context_window();
    // The gauge is the telemetry: the glyph itself fills with the
    // window, readable before the numbers are.
    let fraction = limit
        .map(|limit| used as f64 / limit as f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let mut context = format_tokens(Some(used));
    if let Some(limit) = limit {
        context.push_str(&format!("/{}", format_tokens(Some(limit))));
        // The fill as a number beside the glyph: the glyph shows the
        // bucket, the percentage the exact read.
        context.push_str(&format!(" ({:.1}%)", fraction * 100.0));
    }
    parts
        .push(Span::raw(format!(" · {} {context}", (app.icons.context)(fraction))).fg(theme.text3));
    parts.push(Span::raw(format!(" · {}", cost_text(app.session_cost))).fg(theme.text3));
    // The plan's remaining allowance, when the active backend reports
    // one: the adapter owns the figures, the footer only prints them.
    // A kind names the unit where the amount alone would read as
    // noise ("238/300"); a money amount needs no word beside it.
    let active = app.backend.as_ref().map(crate::app::Backend::config_name);
    if let Some((provider, Some(allowance))) = &app.subscription {
        if Some(*provider) == active {
            let mut badge = format!("{} {}", app.icons.allowance, allowance.amount);
            if !allowance.kind.is_empty() {
                badge.push(' ');
                badge.push_str(&allowance.kind);
            }
            parts.push(Span::raw(format!(" · {badge}")).fg(theme.text3));
        }
    }
    if app.chat.scroll > 0 && app.error.is_none() {
        parts.push(Span::raw(" · viewing history").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(Line::from(parts).left_aligned()), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // The input is the one interactive element, so it earns the accent;
    // while a turn runs it calms to a hairline.
    let border = if app.turn.running {
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

    // The full inner width: `input_layout` subtracts the marker gutter
    // itself, and `input_rows` sizes the box from the same border-minus-
    // gutter width. Wrapping narrower here than there would clip the last
    // row under the box until the next keystroke grew it.
    let width = inner.width as usize;
    let prompt = "❯ ";
    // The layout is the render's source of truth: text rows with the
    // anchors inline, as chips, exactly as they sit in the input.
    let layout = input_layout(&app.chat.input, &app.chat.attachments, width, theme);
    let cursor_at = cursor_at_rows(&layout, app.chat.cursor);
    // The window keeps the cursor's row visible; with no cursor (a
    // running turn) the tail shows.
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
    if !app.turn.running {
        // The prompt and the continuation indent are both two cells,
        // and only rows carrying one shift the cursor by them. A cursor
        // past a full row's last cell clamps onto it — there is no
        // column beyond the box.
        let marker = u16::from(layout[cursor_row].marker);
        frame.set_cursor_position(Position {
            x: (inner.x + marker * 2 + cursor_col as u16).min(inner.right().saturating_sub(1)),
            y: inner.y + cursor_row.saturating_sub(first) as u16,
        });
    }
}

/// Content rows the input box may grow to; past that, a window keeps the
/// cursor's line shown.
const INPUT_MAX_CONTENT_ROWS: usize = 8;

/// One visual row of the input box: the rendered line plus the
/// bookkeeping the cursor mapping and the height math read.
#[derive(Debug, Clone)]
struct InputRow {
    line: Line<'static>,
    /// Source characters of `input` this row accounts for — a chip
    /// accounts for its single anchor character.
    source_chars: usize,
    /// Placed chips as `(source offset, display column, display width)`
    /// within the row's text: a chip is one source step but spans
    /// several cells, and the cursor mapping crosses it in one step.
    chips: Vec<(usize, usize, usize)>,
    /// Whether the row carries the two-cell logical-line marker.
    marker: bool,
    /// Whether the row ends its logical line — the newline character is
    /// one cursor step the cursor mapping must account for.
    newline: bool,
}

/// One laid-out unit of an input row.
#[derive(Debug)]
enum Segment {
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
struct RowSegs {
    spans: Vec<Span<'static>>,
    source: usize,
    chips: Vec<(usize, usize, usize)>,
}

/// Word-wraps laid-out segments to a display width, mirroring
/// [`wrap_line`]: a whitespace run never opens a row, an overflowing
/// one is dropped, a word wider than the row hard-splits. A chip moves
/// whole instead of splitting.
fn wrap_segments(segments: Vec<Segment>, width: usize) -> Vec<RowSegs> {
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

/// A chip's inline label: `▣1 5L` — the anchor's one-based id and the
/// payload's line count.
fn chip_label(paste: &str, index: usize) -> String {
    format!("▣{} {}L", index + 1, paste.lines().count())
}

/// One attachment chip's span: the label padded one cell each side on
/// a surface-colored card — a thing apart from the typed text around
/// it, no border glyphs needed.
fn chip_spans(paste: &str, index: usize, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!(" {} ", chip_label(paste, index)),
        Style::default().fg(theme.info).bg(theme.surface),
    )]
}

/// Maps the cursor's source index onto (row, column) of the layout.
/// Columns are display columns of the row's text — the render adds the
/// marker's two cells. Chips passed on the way widen the column; a
/// cursor sitting at a chip's source position lands on its first cell,
/// so the whole label crosses in one step.
fn cursor_at_rows(layout: &[InputRow], cursor: usize) -> Option<(usize, usize)> {
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
fn input_layout(input: &str, attachments: &[String], width: usize, theme: &Theme) -> Vec<InputRow> {
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
                    attachments.get(anchor).map(String::as_str).unwrap_or(""),
                    anchor,
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
fn input_rows(input: &str, attachments: &[String], width: u16, theme: &Theme) -> u16 {
    let width = width.saturating_sub(2).max(1) as usize;
    (input_layout(input, attachments, width, theme)
        .len()
        .clamp(1, INPUT_MAX_CONTENT_ROWS)
        + 2) as u16
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
    use crate::screens::{CatalogModel, PickerPurpose, PickerScreen};
    use ratatui::{Terminal, backend::TestBackend};
    use rhema_provider::ModelCard;

    fn model(id: &str) -> ModelCard {
        ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }

    /// The Usage page's detailed split: the subtitle, the six rows as
    /// percentages of the total with their kilo-scaled amounts.
    #[test]
    fn the_usage_page_shows_the_detailed_split() {
        let mut settings =
            crate::screens::SettingsScreen::new(crate::screens::SettingsPurpose::FromChat);
        settings.section = SettingsSection::Usage;
        settings.transcript = Some(rhema_transcript::TranscriptStats {
            requests: 10,
            viewed: 4,
            flushes: 1,
            input_tokens: 90_000,
            cached_input_tokens: 70_000,
            output_tokens: 10_000,
            usage: rhema_transcript::UsageBreakdown {
                system_prompt_tokens: 20_000,
                skills_tokens: 2_000,
                tool_calls_input_tokens: 30_000,
                tool_calls_output_tokens: 30_000,
                user_messages_tokens: 8_000,
                model_output_tokens: 10_000,
            },
            ..Default::default()
        });

        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        let text: String = (0..40)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(text.contains("Detailed session usage"), "{text}");
        assert!(text.contains("20% (20k)"), "{text}");
        assert!(text.contains("2% (2k)"), "{text}");
        assert!(text.contains("30% (30k)"), "{text}");
        assert!(text.contains("8% (8k)"), "{text}");
        assert!(text.contains("10% (10k)"), "{text}");
    }

    /// The settings screen draws the whole sidebar with the selected
    /// section marked, and the Providers page carries the live instance
    /// list with its add row.
    #[test]
    fn the_settings_screen_draws_the_sidebar_and_the_page() {
        let mut settings =
            crate::screens::SettingsScreen::new(crate::screens::SettingsPurpose::FromChat);
        settings.profile = "personal".into();
        settings.stored.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: Some("k".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        settings.stored.provider = Some("google".into());
        settings.stored.instance = Some("google".into());

        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        let rows: Vec<String> = (0..16)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let text = rows.join("\n");
        // The border is whole: no title breaks the top edge, and the
        // gear heads the sidebar instead.
        assert!(!rows[0].contains("settings"), "no border title: {text}");
        assert!(rows[0].trim().starts_with('╭'), "full border: {text}");
        assert!(text.contains("⚙"), "the gear heads the sidebar: {text}");
        assert!(text.contains("Settings"), "the sidebar heading: {text}");
        for section in SettingsSection::ALL {
            assert!(
                text.contains(section.label()),
                "{} sits in the sidebar: {text}",
                section.label()
            );
        }
        assert!(text.contains("❯ Providers"), "the selection marked: {text}");
        // The content pane opens with the section's own title over a
        // rule, aligned with the sidebar heading's row.
        let heading_row = rows.iter().find(|row| row.contains("⚙")).expect("gear row");
        assert!(
            heading_row.contains("Providers"),
            "the content titles beside the heading: {heading_row}"
        );
        assert!(text.contains("Profile"), "the page's rows: {text}");
        assert!(text.contains("personal"), "{text}");
        assert!(
            text.contains("google (google) active"),
            "the active instance row: {text}"
        );
        assert!(
            text.contains("+ Add a new instance..."),
            "the add row: {text}"
        );
        assert!(
            text.contains("d - delete provider"),
            "the footer hints: {text}"
        );
        // Every sidebar row plus the instance and add rows are clickable.
        assert_eq!(
            hits.len(),
            SettingsSection::ALL.len() + 2,
            "sidebar rows and the two page rows register: {hits:?}"
        );
    }

    /// The startup gate's explanation renders over the page the
    /// settings open on, wrapped, and survives a section change.
    #[test]
    fn the_startup_reason_rides_over_the_page() {
        let mut settings =
            crate::screens::SettingsScreen::new(crate::screens::SettingsPurpose::Initial);
        settings.startup_reason = Some(
            "missing API key: set GEMINI_API_KEY — the system keychain is \
             locked or unreachable, so the stored keys were unreadable"
                .to_owned(),
        );

        let draw = |settings: &crate::screens::SettingsScreen| {
            let mut hits = Vec::new();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_settings(frame, settings, &DARK, &crate::icons::UNICODE, &mut hits)
                })
                .unwrap();
            (0..24)
                .map(|y| {
                    (0..80)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let text = draw(&settings);
        assert!(
            text.contains("GEMINI_API_KEY"),
            "the reason shows on the opening page: {text}"
        );
        assert!(
            text.contains("locked or unreachable"),
            "the reason wraps to the pane: {text}"
        );

        settings.section = SettingsSection::About;
        let text = draw(&settings);
        assert!(
            text.contains("GEMINI_API_KEY"),
            "the reason rides over the other pages too: {text}"
        );
    }

    /// The add dialog pops over the settings: its provider list draws,
    /// and its rows register as click targets while it shows.
    #[test]
    fn the_add_dialog_draws_its_provider_list() {
        let mut settings =
            crate::screens::SettingsScreen::new(crate::screens::SettingsPurpose::FromChat);
        settings.dialog = Some(crate::screens::AddInstanceDialog::new(&settings.stored));

        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        let rows: Vec<String> = (0..16)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let text = rows.join("\n");
        assert!(text.contains("add a login"), "the dialog title: {text}");
        assert!(
            text.contains("GitHub Copilot"),
            "the provider list renders: {text}"
        );
        let provider_hits = hits
            .iter()
            .filter(|(_, hit)| matches!(hit, SettingsHit::DialogProvider(_)))
            .count();
        assert_eq!(provider_hits, PROVIDERS.len(), "each provider is clickable");
    }

    /// The selection must be visible: the highlighted row carries the
    /// marker, the others do not.
    #[test]
    fn the_selected_picker_row_shows_the_marker() {
        let picker = PickerScreen::new(
            vec![model("model-a"), model("model-b")]
                .into_iter()
                .map(|model| CatalogModel {
                    provider: "google",
                    instance: None,
                    model,
                })
                .collect(),
            PickerPurpose::Initial,
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

    /// A provider whose fetch failed while others answered is named in
    /// the footer: a missing catalog must not read as an empty one.
    #[test]
    fn the_footer_names_the_failed_providers() {
        let mut picker = PickerScreen::new(
            vec![model("model-a")]
                .into_iter()
                .map(|model| CatalogModel {
                    provider: "google",
                    instance: None,
                    model,
                })
                .collect(),
            PickerPurpose::Initial,
        );
        picker.failed = vec!["openrouter"];

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker, &DARK))
            .unwrap();
        let footer: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 10)].symbol())
            .collect();
        assert!(footer.contains("no models from openrouter"), "{footer}");
    }

    /// A selection scrolled to the bottom of a long list must land on the
    /// last model row, never on the footer row underneath the list.
    #[test]
    fn the_marker_never_lands_on_the_footer_row() {
        let models: Vec<CatalogModel> = (0..20)
            .map(|i| model(&format!("model-{i:02}")))
            .map(|model| CatalogModel {
                provider: "google",
                instance: None,
                model,
            })
            .collect();
        let mut picker = PickerScreen::new(models, PickerPurpose::Initial);
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

    /// The footer reads the bare model name: an OpenRouter id's vendor
    /// prefix is cut, plain ids pass through.
    #[test]
    fn the_footer_model_cuts_the_vendor_prefix() {
        assert_eq!(
            footer_model("apodex/apodex-1.1-mini:free"),
            "apodex-1.1-mini:free"
        );
        assert_eq!(footer_model("gemini-3.8-flash"), "gemini-3.8-flash");
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
    use super::{
        cursor_at_rows, elapsed_text, footer_folder, input_layout, input_rows, render, wrap_line,
    };
    use crate::app::{App, AppSetup, RunOptions};
    use crate::events::OwnedEvent;
    use crate::palette::DARK;
    use crate::palette::ThemeKind;
    use ratatui::text::Line;
    use std::collections::BTreeMap;

    use crate::screens::{CatalogModel, ChatState};
    use crate::store::MemoryStore;
    use ratatui::{Terminal, backend::TestBackend};
    use rhema_core::Usage;
    use rhema_provider::ModelCard;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn chat_app() -> App {
        let mut personal = rhema_config::Profile {
            model: Some("m".into()),
            ..rhema_config::Profile::default()
        };
        personal.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: Some("k".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        let config = rhema_config::Config {
            profiles: BTreeMap::from([("personal".to_owned(), personal)]),
            ..Default::default()
        };
        App::new(AppSetup {
            store: Arc::new(MemoryStore::default()),
            config,
            backend: Some(crate::app::Backend::Google {
                api_key: "k".into(),
            }),
            session: rhema_core::Session::default(),
            secrets: None,
            sandboxed: None,
            sandbox_offered: crate::sandbox::SandboxAvailability::default(),
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: ThemeKind::Dark,
                m3_source: None,
                custom: None,
                icons: crate::icons::Flavor::Unicode,
                thinking: rhema_core::request::ThinkingEffort::Default,
                profile: Some("personal".into()),
                workspace: PathBuf::from("."),
                max_steps: 4,
                no_transcript: false,
                base_system: None,
                save: None,
            },
        })
        .unwrap()
    }

    fn screen_text(app: &mut App) -> String {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect()
    }

    #[test]
    fn elapsed_time_reads_as_a_clock() {
        assert_eq!(elapsed_text(std::time::Duration::from_secs(42)), "42s");
        assert_eq!(
            elapsed_text(std::time::Duration::from_secs(2055)),
            "34min 15s"
        );
        assert_eq!(
            elapsed_text(std::time::Duration::from_secs(3900)),
            "1h 5min"
        );
    }

    /// A working subagent pins its live line under the spinner: the
    /// delegate's name, its phase, its spend; the result takes it off.
    #[test]
    fn a_working_subagent_pins_its_line_under_the_spinner() {
        let mut app = chat_app();
        app.turn.running = true;
        app.turn.processing = true;
        let on = |app: &mut App, event| {
            app.on_event(crate::events::UiEvent::Engine(event));
        };
        on(
            &mut app,
            OwnedEvent::ToolCallStart {
                id: "c1".into(),
                name: "subagent".into(),
            },
        );
        on(
            &mut app,
            OwnedEvent::ToolCallArgsDelta {
                id: "c1".into(),
                delta: r#"{"name": "general-purpose", "prompt": "explore"}"#.into(),
            },
        );
        on(
            &mut app,
            OwnedEvent::ToolStatus {
                id: "c1".into(),
                status: rhema_core::ToolStatus {
                    phase: "writing output".into(),
                    usage: Some(rhema_core::Usage {
                        input_tokens: 45_000,
                        output_tokens: 500,
                        cached_input_tokens: 0,
                    }),
                    model: Some("m".into()),
                },
            },
        );
        // Wide enough that the pinned line never clips: the strip does
        // not wrap, a narrow screen would cut the figures off.
        let screen = {
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };
        assert!(screen.contains("general-purpose"), "{screen}");
        assert!(screen.contains("writing output"), "{screen}");
        assert!(screen.contains("45k"), "{screen}");
        assert!(screen.contains("0s"), "{screen}");

        // The report landed: the live line goes, nothing pinned.
        on(
            &mut app,
            OwnedEvent::ToolResult {
                id: "c1".into(),
                content: "done".into(),
                is_error: false,
                code: None,
            },
        );
        let screen = screen_text(&mut app);
        assert!(!screen.contains("writing output"), "{screen}");
    }

    /// A subagent call's transcript row reads as delegation: the robot
    /// badge, the mark, the subagent and its task's head.
    #[test]
    fn a_subagent_call_renders_as_delegation() {
        let mut app = chat_app();
        app.transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "subagent".into(),
        });
        app.transcript.apply(&OwnedEvent::ToolCallArgsDelta {
            id: "c1".into(),
            delta: r#"{"name": "general-purpose", "prompt": "explore"}"#.into(),
        });
        app.transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "done".into(),
            is_error: false,
            code: None,
        });
        let screen = screen_text(&mut app);
        assert!(screen.contains("general-purpose — explore"), "{screen}");
        assert!(screen.contains("✓"), "{screen}");
    }

    /// The providers page reports the stored key state: one row per
    /// instance with its provider, the active one marked, the add row
    /// closing the list.
    #[test]
    fn the_providers_page_reports_the_stored_key_state() {
        let mut settings =
            crate::screens::SettingsScreen::new(crate::screens::SettingsPurpose::FromChat);
        settings.stored.instances.insert(
            "home".to_owned(),
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some("http://home:8080".into()),
            },
        );
        settings.stored.instances.insert(
            "work sub".to_owned(),
            rhema_config::ProviderInstance {
                provider: "copilot".into(),
                api_key: Some("g".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        settings.stored.provider = Some("copilot".into());
        settings.stored.instance = Some("work sub".into());

        let backend = TestBackend::new(60, 17);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                crate::ui::render_settings(
                    frame,
                    &settings,
                    &crate::palette::DARK,
                    &crate::icons::UNICODE,
                    &mut hits,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(screen.contains("home (llama-cpp)"), "{screen}");
        assert!(screen.contains("work sub (copilot) active"), "{screen}");
        assert_eq!(screen.matches(" active").count(), 1, "{screen}");
        assert!(screen.contains("+ Add a new instance..."), "{screen}");
        // One click target per rendered row: the three instance rows.
        let page_rows = hits
            .iter()
            .filter(|(_, hit)| matches!(hit, crate::app::SettingsHit::PageRow(_)))
            .count();
        assert_eq!(page_rows, 3, "two instances plus the add row: {hits:?}");
    }

    /// The flow's instruction wraps: a device-grant URL and its code
    /// exceed the box's width, and a cut-off code cannot be typed.
    #[test]
    fn the_flow_instruction_wraps_instead_of_cutting_off() {
        let mut dialog = crate::screens::AddInstanceDialog::new(&rhema_config::Profile::default());
        dialog.stage = crate::screens::DialogStage::Flow;
        dialog.flow_instruction = Some(rhema_provider::LoginInstruction {
            text: "open https://github.com/login/device in a browser and enter the code there"
                .into(),
            url: None,
            code: Some("ABCD-1234".into()),
        });
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                crate::ui::render_add_dialog(frame, &dialog, &crate::palette::DARK, &mut Vec::new())
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        // The text's tail made it onto a wrapped second line, and the
        // code renders whole below it.
        assert!(screen.contains("enter the"), "{screen}");
        assert!(screen.contains("code there"), "{screen}");
        assert!(screen.contains("ABCD-1234"), "{screen}");
    }

    #[test]
    fn palette_descriptions_align_in_one_column() {
        let mut app = chat_app();
        app.chat.input = "/".to_owned();
        app.chat.cursor = 1;

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };

        // Each command's description starts at the same column: the
        // longest name plus the three-space gap.
        let mut columns = Vec::new();
        for y in 0..buffer.area.height {
            let row = row_text(y);
            if let Some(pos) = row.find("commands and keys") {
                columns.push(("commands and keys", pos));
            }
            if let Some(pos) = row.find("pick the model") {
                columns.push(("pick the model", pos));
            }
            if let Some(pos) = row.find("reasoning effort") {
                columns.push(("reasoning effort", pos));
            }
        }
        assert_eq!(columns.len(), 3, "all searched rows visible: {columns:?}");
        assert!(
            columns.iter().all(|(_, pos)| *pos == columns[0].1),
            "descriptions misaligned: {columns:?}"
        );
    }

    /// The footer's folder: the home and its `Documents` trim away, and
    /// at most two parents ride beside the folder itself — three
    /// folders in all.
    #[test]
    fn the_footer_folder_trims_home_and_caps_parents() {
        let home = Path::new("/home/u");
        // The home-trimmed workspace reads as its projects path.
        assert_eq!(
            footer_folder(Path::new("/home/u/Documents/prjs/rhema"), Some(home)),
            (String::new(), vec!["prjs".to_owned()], "rhema".to_owned())
        );
        // Deeper than three folders: what fell off leaves the ellipsis.
        assert_eq!(
            footer_folder(Path::new("/home/u/prjs/a/b/c/d/e"), Some(home)),
            (
                "…/".to_owned(),
                vec!["c".to_owned(), "d".to_owned()],
                "e".to_owned()
            )
        );
        // Outside the home, a whole absolute path keeps its root slash.
        assert_eq!(
            footer_folder(Path::new("/var/log/app"), None),
            (
                "/".to_owned(),
                vec!["var".to_owned(), "log".to_owned()],
                "app".to_owned()
            )
        );
        // Nothing left after the trim: the name itself.
        assert_eq!(
            footer_folder(Path::new("/home/u"), Some(home)),
            (String::new(), Vec::new(), "u".to_owned())
        );
        assert_eq!(
            footer_folder(Path::new("/home/u/Documents"), Some(home)),
            (String::new(), Vec::new(), "Documents".to_owned())
        );
        // Filesystem root: a bare slash, no parents.
        assert_eq!(
            footer_folder(Path::new("/"), Some(home)),
            (String::new(), Vec::new(), "/".to_owned())
        );
    }

    /// The footer carries the folder badge: the trimmed workspace path
    /// with its folder at the end, the model a separator behind it.
    #[test]
    fn the_footer_shows_the_workspace_folder() {
        let mut app = chat_app();
        // The harness profile names no provider, so the footer would
        // take the login hint; a provider renders the model instead.
        app.config.profiles.get_mut("personal").unwrap().provider = Some("google".to_owned());
        app.home = Some(PathBuf::from("/home/u"));
        app.options.workspace = PathBuf::from("/home/u/Documents/prjs/rhema");
        let screen = screen_text(&mut app);
        assert!(screen.contains("prjs/rhema · m"), "{screen}");
        // Segments sit one separator apart, never two.
        assert!(!screen.contains("·  ·"), "{screen}");
    }

    /// The allowance badge shows only the active backend's report: a
    /// fetch from a backend the session has since left never shows. A
    /// money amount rides bare — the icon says the unit — while a
    /// metered quota keeps its word.
    #[test]
    fn the_footer_shows_only_the_active_backend_allowance() {
        let mut app = chat_app();
        app.subscription = Some((
            "openrouter",
            Some(rhema_provider::Allowance {
                amount: "$4.12".to_owned(),
                kind: String::new(),
            }),
        ));
        // The report names openrouter; the session rides google.
        assert!(!screen_text(&mut app).contains("$4.12"));

        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };
        app.backend = Some(crate::app::Backend::OpenRouter {
            api_key: "k".into(),
        });
        let screen = draw(&mut app);
        assert!(screen.contains("¤ $4.12"), "badge shows: {screen}");
        assert!(
            !screen.contains("$4.12 credit"),
            "no word on money: {screen}"
        );

        // A metered quota names its unit; the word is the read.
        app.subscription = Some((
            "openrouter",
            Some(rhema_provider::Allowance {
                amount: "238/300".to_owned(),
                kind: "premium".to_owned(),
            }),
        ));
        assert!(draw(&mut app).contains("¤ 238/300 premium"), "kind shows");
    }

    #[test]
    fn the_footer_shows_the_thinking_and_context_icons() {
        let mut app = chat_app();
        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };
        assert!(!draw(&mut app).contains("⁂ H"), "absent at default");
        assert!(
            draw(&mut app).contains("◔ 0"),
            "the empty gauge still shows"
        );

        // A named level rides with the thinking glyph, and the gauge
        // fills with the window's usage.
        app.options.thinking = rhema_core::request::ThinkingEffort::High;
        app.models.push(CatalogModel {
            provider: "google",
            instance: None,
            model: ModelCard {
                id: "m".into(),
                display_name: "m".into(),
                description: String::new(),
                input_token_limit: Some(1_048_576),
                output_token_limit: None,
                generation_methods: vec!["generateContent".to_owned()],
                pricing: None,
            },
        });
        app.usage = Some(Usage {
            input_tokens: 131_072,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
        let screen = draw(&mut app);
        assert!(screen.contains("⁂ H"), "{screen}");
        // 131_072 of 1_048_576 is ~12%: the emptiest quarter.
        assert!(screen.contains("◔ 131k/1.0M (12.5%)"), "{screen}");
    }

    #[test]
    fn the_footer_names_the_active_profile_in_its_color() {
        let mut app = chat_app();
        app.chat.input = "/".to_owned();
        app.chat.cursor = 1;
        // chat_app runs the personal profile: the seeded default tone.
        let (name, color) = app.active_profile().expect("seeded profile");
        assert_eq!(name, "personal");
        assert_eq!(color, crate::palette::DARK.accent_text);

        // A configured color wins; unknown names read secondary.
        app.session_profile = Some("work".into());
        app.config.profiles.insert(
            "work".to_owned(),
            rhema_config::Profile {
                color: Some("#D9A643".into()),
                ..rhema_config::Profile::default()
            },
        );
        let (name, color) = app.active_profile().expect("configured profile");
        assert_eq!(name, "work");
        assert_eq!(color, ratatui::style::Color::Rgb(0xD9, 0xA6, 0x43));
    }

    #[test]
    fn the_footer_admits_when_a_profile_has_no_provider() {
        let mut app = chat_app();
        app.session_profile = Some("work".into());
        app.config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(screen.contains("no provider; use /login"), "{screen}");
        assert!(
            !screen.contains("Qwen"),
            "another profile's model must not leak: {screen}"
        );
    }

    #[test]
    fn long_lines_wrap_and_scroll_reaches_the_top() {
        let mut app = chat_app();
        let long = format!("{} end-marker", "word ".repeat(200));
        app.transcript.push_user(&long);

        // At the tail, the last word is visible.
        let text = screen_text(&mut app);
        assert!(text.contains("end-marker"), "{text}");

        // Scrolled far back — far past the top, even — the draw clamps
        // the offset to the content, and the first words are visible.
        app.chat.scroll = 1000;
        let text = screen_text(&mut app);
        assert!(text.contains("word word word"), "{text}");
        assert!(!text.contains("end-marker"), "{text}");
        // The clamp is stable: redrawing does not move it further.
        let clamped = app.chat.scroll;
        let text = screen_text(&mut app);
        assert_eq!(app.chat.scroll, clamped);
        assert!(text.contains("word word word"), "{text}");
    }

    #[test]
    fn a_long_paste_renders_as_one_inline_cell() {
        let mut app = chat_app();
        app.chat.paste("l1\nl2\nl3\nl4\nl5");
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(
            screen.contains("❯  ▣1 5L"),
            "the chip sits on the prompt row: {screen}"
        );
        assert!(!screen.contains("l1"), "the body stays hidden: {screen}");
    }

    #[test]
    fn a_submitted_paste_renders_as_a_chip_in_the_transcript() {
        let mut app = chat_app();
        let long = "l1\nl2\nl3\nl4\nl5";
        app.transcript.push_user(&rhema_core::paste::embed(
            &format!("look at {} this", ChatState::MARK),
            &[long.to_owned()],
        ));
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        // The chip rides inline in the prompt; the payload's body
        // and its newlines never show.
        assert!(screen.contains("look at  ▣1 5L  this"), "{screen}");
        assert!(!screen.contains("l1"), "{screen}");
    }

    /// A running command's output earns the preview at a second line —
    /// an instant command never flashes — or at a second of runtime,
    /// and goes when the result lands.
    #[test]
    fn a_running_command_previews_its_lines_once_earned() {
        let mut app = chat_app();
        app.turn.running = true;
        app.turn.processing = true;
        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };
        let chunk = |app: &mut App, id: &str, text: &str| {
            app.on_event(crate::events::UiEvent::Engine(
                crate::events::OwnedEvent::ToolOutputDelta {
                    id: id.into(),
                    chunk: text.into(),
                },
            ));
        };

        // One fresh line: the command has not earned the screen yet.
        chunk(&mut app, "c1", "compiling foo\n");
        assert!(!draw(&mut app).contains("compiling foo"));

        // A second line earns it immediately.
        chunk(&mut app, "c1", "linking bar\n");
        assert!(draw(&mut app).contains("linking bar"));

        // One line earns it once the command has run long enough.
        let mut app = chat_app();
        app.turn.running = true;
        app.turn.processing = true;
        chunk(&mut app, "c1", "compiling foo\n");
        app.turn.tool_output_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        assert!(draw(&mut app).contains("compiling foo"));

        // The result takes the preview back off.
        app.on_event(crate::events::UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(!draw(&mut app).contains("compiling foo"));
    }

    #[test]
    fn the_status_line_names_the_phase() {
        let mut app = chat_app();
        app.turn.running = true;
        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };

        app.turn.processing = false;
        let screen = draw(&mut app);
        assert!(screen.contains("thinking"), "{screen}");

        app.turn.processing = true;
        let screen = draw(&mut app);
        assert!(screen.contains("running"), "{screen}");
        assert!(!screen.contains("thinking"), "{screen}");
    }

    #[test]
    fn the_reasoning_preview_shows_and_vanishes_with_output() {
        let mut app = chat_app();
        app.turn.running = true;
        // The line ends with its newline: the preview only shows
        // completed lines.
        app.turn.thinking = "reasoning about the tree state\n".to_owned();
        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };

        let screen = draw(&mut app);
        assert!(screen.contains("reasoning about"), "{screen}");

        // Cleared buffer, cleared block: the strip is back to the bare
        // spinner.
        app.turn.thinking.clear();
        let screen = draw(&mut app);
        assert!(!screen.contains("reasoning about"), "{screen}");
    }

    /// The preview reveals a line only once its newline has streamed:
    /// the row still in progress stays hidden until it completes.
    #[test]
    fn the_reasoning_preview_reveals_complete_lines_only() {
        let mut app = chat_app();
        app.turn.running = true;
        let draw = |app: &mut App| {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };

        app.turn.thinking = "first thought\nhalf a second".to_owned();
        let screen = draw(&mut app);
        assert!(screen.contains("first thought"), "{screen}");
        assert!(!screen.contains("half a second"), "{screen}");

        // The newline lands: the held-back line shows whole.
        app.turn.thinking.push('\n');
        let screen = draw(&mut app);
        assert!(screen.contains("half a second"), "{screen}");
    }

    #[test]
    fn the_spinner_sits_below_a_blank_row_and_on_the_transcript_inset() {
        let mut app = chat_app();
        app.turn.running = true;
        app.transcript.push_user("look at this");

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let spinner_row = (0..buffer.area.height)
            .find(|y| row_text(*y).contains("thinking"))
            .expect("spinner visible");
        assert!(
            row_text(spinner_row).starts_with(" ⠋"),
            "indented one cell: {:?}",
            row_text(spinner_row)
        );
        assert!(
            row_text(spinner_row - 1).trim_end().is_empty(),
            "a blank row above: {:?}",
            row_text(spinner_row - 1)
        );
    }

    /// Typing `/model ` must raise the model palette over the input:
    /// the rows come from the cached catalog, and tab completes into it.
    #[test]
    fn the_model_palette_renders_above_the_input() {
        use crossterm::event::{KeyCode, KeyModifiers};

        fn key(code: KeyCode) -> crossterm::event::KeyEvent {
            crossterm::event::KeyEvent::new(code, KeyModifiers::empty())
        }
        let mut app = chat_app();
        app.on_event(crate::events::UiEvent::ModelsLoaded {
            why: crate::events::FetchWhy::Bootstrap,
            result: vec![crate::events::CatalogFetch {
                provider: "google",
                instance: None,
                result: Ok(vec![ModelCard {
                    id: "gemini-3.8-flash".into(),
                    display_name: "Gemini 3.8 Flash".into(),
                    description: String::new(),
                    input_token_limit: Some(1_048_576),
                    output_token_limit: None,
                    generation_methods: vec!["generateContent".into()],
                    pricing: None,
                }]),
            }],
        });
        assert!(!app.models.is_empty(), "the catalog is cached");
        for c in "/model ".chars() {
            app.chat.handle_key(key(KeyCode::Char(c)), &app.models, &[]);
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(
            screen.contains("google/gemini-3.8-flash"),
            "the model row is on screen: {screen}"
        );
    }

    /// Typing `/thinking ` must raise the level palette over the input:
    /// one row per level, the description dimmed beside it.
    #[test]
    fn the_thinking_palette_renders_above_the_input() {
        use crossterm::event::{KeyCode, KeyModifiers};

        fn key(code: KeyCode) -> crossterm::event::KeyEvent {
            crossterm::event::KeyEvent::new(code, KeyModifiers::empty())
        }
        let mut app = chat_app();
        for c in "/thinking ".chars() {
            app.chat.handle_key(key(KeyCode::Char(c)), &app.models, &[]);
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(
            screen.contains("medium") && screen.contains("the default effort"),
            "the level rows are on screen: {screen}"
        );
    }

    #[test]
    fn the_palette_window_scrolls_with_the_selection() {
        use crossterm::event::{KeyCode, KeyModifiers};

        fn key(code: KeyCode) -> crossterm::event::KeyEvent {
            crossterm::event::KeyEvent::new(code, KeyModifiers::empty())
        }
        let mut app = chat_app();
        app.chat.input = "/".to_owned();
        app.chat.cursor = 1;
        // Fourteen commands, six visible rows: nine downs put the
        // selection on /resume (the /sandbox row sits beside /profile
        // now), and the window must have followed it.
        for _ in 0..9 {
            app.chat.handle_key(key(KeyCode::Down), &[], &[]);
        }
        assert_eq!(app.chat.palette_index, 9);
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(
            screen.contains("/resume"),
            "the selection is shown: {screen}"
        );
        assert!(!screen.contains("/help"), "the head scrolled out: {screen}");

        // And back to the top restores it.
        app.chat.palette_index = 0;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(screen.contains("/help"), "{screen}");
    }

    #[test]
    fn the_input_box_grows_and_caps() {
        assert_eq!(input_rows("", &[], 40, &DARK), 3, "empty: one content row");
        assert_eq!(input_rows("one line", &[], 40, &DARK), 3);
        assert_eq!(input_rows("a\nb\nc", &[], 40, &DARK), 5, "one row per line");
        // Chips render inline: two of them between text cost no rows of
        // their own.
        let marked = format!("a{}{}b", ChatState::MARK, ChatState::MARK);
        let pastes = vec!["x\ny\nz\nw".to_owned(); 2];
        assert_eq!(input_rows(&marked, &pastes, 40, &DARK), 3);
        let wall = "x\n".repeat(20);
        assert_eq!(input_rows(&wall, &[], 40, &DARK), 10, "the cap holds");
    }

    #[test]
    fn the_cursor_maps_across_newlines_and_anchors() {
        let layout = input_layout("ab\ncd", &[], 40, &DARK);
        // End of the first line, start of the second, end of the
        // second: the newline itself is one step between them.
        assert_eq!(cursor_at_rows(&layout, 2), Some((0, 2)));
        assert_eq!(cursor_at_rows(&layout, 3), Some((1, 0)));
        assert_eq!(cursor_at_rows(&layout, 5), Some((1, 2)));

        // A chip is one step whatever its label's width: the cursor
        // lands on the border's first cell, then crosses the whole chip
        // in one step onto the next character.
        let marked = format!("a{}b", ChatState::MARK);
        let layout = input_layout(&marked, &["x\ny".to_owned()], 40, &DARK);
        assert_eq!(cursor_at_rows(&layout, 1), Some((0, 1)), "on the chip");
        assert_eq!(
            cursor_at_rows(&layout, 2),
            Some((0, 8)),
            "after the chip: chip width crossed in one step"
        );
    }

    #[test]
    fn chips_number_across_lines_with_their_own_payload() {
        // Anchors on different logical lines: the ids run across the
        // whole input, and each chip shows its own paste's size.
        let input = format!("a{}\nb{}", ChatState::MARK, ChatState::MARK);
        let rows = input_layout(
            &input,
            &["l1\nl2\nl3\nl4".to_owned(), "m1\nm2\nm3\nm4\nm5".to_owned()],
            40,
            &DARK,
        );
        let rendered: Vec<String> = rows
            .iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert!(rendered[0].contains(" ▣1 4L "), "{rendered:?}");
        assert!(rendered[1].contains(" ▣2 5L "), "{rendered:?}");
    }

    #[test]
    fn a_chip_never_splits_across_rows() {
        // The chip sits exactly past the wrap edge: it moves whole to
        // the next row rather than splitting.
        let marked = format!("wordy {}", ChatState::MARK);
        let layout = input_layout(&marked, &["x\ny".to_owned()], 12, &DARK);
        assert_eq!(layout.len(), 2, "the word's row, then the chip alone");
        let chip: String = layout[1]
            .line
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(chip, "   ▣1 2L ");
        // The boundary binds to the earlier row, as with a newline; one
        // step on and the cursor has crossed the whole chip.
        assert_eq!(cursor_at_rows(&layout, 6), Some((0, 6)));
        assert_eq!(cursor_at_rows(&layout, 7), Some((1, 7)), "past the chip");
    }

    #[test]
    fn attachment_chips_render_inline_where_they_were_pasted() {
        let input = format!("before{} after", ChatState::MARK);
        let rows = input_layout(&input, &["l1\nl2\nl3\nl4\nl5".to_owned()], 40, &DARK);
        // One row, text and chip in prompt order: the chip costs no row
        // of its own and accounts for its one source character.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_chars, 13);
        let rendered: String = rows[0]
            .line
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(rendered, "❯ before ▣1 5L  after");
        let chip = rows[0]
            .line
            .spans
            .iter()
            .find(|span| span.content.contains("▣1 5L"))
            .expect("the chip renders");
        assert_eq!(chip.style.fg, Some(DARK.info), "the label reads as content");
        assert_eq!(
            chip.style.bg,
            Some(DARK.surface),
            "the label sits on a card"
        );
        // The cursor crosses the seven-cell chip in one step: before it
        // on its first cell, after it on the following character.
        assert_eq!(cursor_at_rows(&rows, 6), Some((0, 6)));
        assert_eq!(cursor_at_rows(&rows, 7), Some((0, 13)));
    }

    /// Dropped whitespace leaves the mapping whole: the wrapper skips
    /// row-leading and overflowing whitespace runs on the display, but
    /// their source characters stay counted, so every cursor offset —
    /// the tail's included — lands on a row and the input renders past
    /// the eight-row cap.
    #[test]
    fn the_input_maps_and_renders_when_whitespace_drops() {
        let mut app = chat_app();
        // A leading space starts a row, so the wrapper drops it; the
        // rest wraps past the eight-row cap.
        app.chat.input = format!(" {}", "lorem ".repeat(60));
        app.chat.cursor = app.chat.input.chars().count();

        let layout = input_layout(&app.chat.input, &[], 36, &DARK);
        assert!(layout.len() > 8, "past the cap: {}", layout.len());
        for offset in 0..=app.chat.cursor {
            assert!(
                cursor_at_rows(&layout, offset).is_some(),
                "offset {offset} maps nowhere"
            );
        }
        assert_eq!(
            cursor_at_rows(&layout, app.chat.cursor).map(|(row, _)| row),
            Some(layout.len() - 1),
            "the tail cursor sits on the last row"
        );

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
    }

    #[test]
    fn overscroll_clamps_so_pgdown_answers_at_once() {
        let mut app = chat_app();
        // Content taller than the viewport: there is a real range.
        app.transcript
            .push_user(&format!("{} end", "word ".repeat(200)));

        // Way past the top: the first draw pulls the offset back into
        // the content's range.
        app.chat.scroll = 50;
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let clamped = app.chat.scroll;
        assert!(clamped > 0 && clamped < 50, "clamped in-range: {clamped}");

        // One PageDown now moves the visible window by the whole tick —
        // no stranded surplus to burn first.
        app.chat.scroll_by(-3);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.chat.scroll, clamped - 3);
    }

    #[test]
    fn a_multi_line_input_renders_every_line_and_caps_at_the_tail() {
        let mut app = chat_app();
        app.chat.insert_str("alpha\nbeta\ngamma");
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        for word in ["alpha", "beta", "gamma"] {
            assert!(screen.contains(word), "{word} visible: {screen}");
        }

        // A wall of text caps the box and shows the tail by the cursor.
        app.chat.insert_str(&"more\n".repeat(12));
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(screen.contains("more"), "{screen}");
        assert!(!screen.contains("alpha"), "the head scrolled out: {screen}");
    }

    #[test]
    fn a_blank_row_separates_a_tool_run_from_the_reply() {
        let mut app = chat_app();
        app.transcript.push_user("investigate");
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            });
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallArgsDelta {
                id: "c1".into(),
                delta: "{\"command\":\"git status\"}".into(),
            });
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolResult {
                id: "c1".into(),
                content: "on main".into(),
                is_error: false,
                code: Some(0),
            });
        app.transcript.apply(&crate::events::OwnedEvent::TextDelta(
            "I've gone through it.".into(),
        ));

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let tool_row = (0..buffer.area.height)
            .find(|y| row_text(*y).contains("$ git status"))
            .expect("tool row visible");
        let reply_row = (tool_row..buffer.area.height)
            .find(|y| row_text(*y).contains("gone through"))
            .expect("reply row visible");
        // Exactly one blank row between the ⌁ line and the response.
        assert_eq!(reply_row, tool_row + 2, "one blank row in between");
    }

    #[test]
    fn a_multi_line_prompt_keeps_its_lines() {
        let mut app = chat_app();
        app.transcript.push_user("alpha\nbeta\ngamma");

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let rows: Vec<String> = (0..buffer.area.height)
            .map(row_text)
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert!(
            rows.contains(&" ❯ alpha".to_owned()),
            "first line keeps the prompt: {rows:?}"
        );
        assert!(
            rows.contains(&" beta".to_owned()) && rows.contains(&" gamma".to_owned()),
            "continuation lines keep their break, bare: {rows:?}"
        );
    }

    #[test]
    fn lines_that_fit_pass_through_without_space_normalization() {
        let line = Line::from("a     b");
        let rows = wrap_line(&line, 10);
        assert_eq!(rows.len(), 1);
        let text: String = rows[0]
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(text, "a     b", "the space run survives");
    }

    #[test]
    fn table_wraps_keep_their_columns_aligned() {
        let mut app = chat_app();
        app.transcript.push_user("what skills");
        // A trigger long enough to wrap inside its column at this
        // terminal width: continuation rows must stay inside the
        // column, not fall back to the row start.
        app.transcript.apply(&OwnedEvent::TextDelta(
            "name | trigger\n--- | ---\ncommit-message | How to write a good commit message and keep it tidy".into(),
        ));

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let continuation = (0..buffer.area.height)
            .map(row_text)
            .find(|row| row.contains("keep it tidy"))
            .expect("the wrapped cell visible");
        // The continuation sits inside the trigger column, indented past
        // the name column's width.
        let indent = continuation.find("keep").unwrap();
        assert!(indent > 14, "indented under its column: {continuation}");
    }

    #[test]
    fn an_empty_session_never_enters_viewing_history() {
        let mut app = chat_app();
        app.chat.scroll = 3;

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        // Nothing to scroll to: the offset clamps away on the draw.
        assert_eq!(app.chat.scroll, 0);
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(!screen.contains("viewing history"), "{screen}");
    }

    #[test]
    fn a_long_prompt_wraps_and_shows_its_tail() {
        let mut app = chat_app();
        let long = format!("{}end", "word ".repeat(30));
        app.chat.insert_str(&long);

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        // The whole prompt is visible, tail included — nothing clips.
        assert!(screen.contains("end"), "{screen}");
        // The box grew to the wrapped height: six-ish rows of content.
        assert!(
            input_rows(&long, &[], 40, &DARK) > 3,
            "one wrapped line, taller box"
        );
    }

    #[test]
    fn the_input_box_grows_exactly_when_the_render_wraps() {
        // The height math and the render must wrap at the same width. A
        // prompt one or two cells past the render's wrap point used to
        // reserve a one-row box and clip its tail — cursor and text
        // hidden below — until further typing pushed past the height
        // math's own wrap point.
        let mut app = chat_app();
        // 36 chars: one row by the height math's wrap (36), two by the
        // render's old, narrower one (34).
        let prompt = format!("{} tail", "a".repeat(31));
        app.chat.insert_str(&prompt);

        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(
            screen.contains("tail"),
            "the whole prompt is visible: {screen}"
        );
    }

    #[test]
    fn a_tool_row_cuts_at_the_width_and_right_aligns_the_code() {
        let mut app = chat_app();
        app.transcript.push_user("investigate");
        let long = "x".repeat(100);
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            });
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallArgsDelta {
                id: "c1".into(),
                delta: format!("{{\"command\":\"{long}\"}}"),
            });
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolResult {
                id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                code: Some(0),
            });

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let row = (0..buffer.area.height)
            .map(row_text)
            .find(|row| row.trim_start().starts_with("$ ") && row.contains('…'))
            .expect("the command cut to the row");
        assert!(row.trim_end().ends_with("[0]"), "code at the end: {row}");
    }

    #[test]
    fn empty_assistant_noise_between_tools_stays_tight() {
        let mut app = chat_app();
        app.transcript.push_user("investigate");
        for (id, command) in [("c1", "git status"), ("c2", "git log")] {
            app.transcript
                .apply(&crate::events::OwnedEvent::ToolCallStart {
                    id: id.into(),
                    name: "bash".into(),
                });
            app.transcript
                .apply(&crate::events::OwnedEvent::ToolCallArgsDelta {
                    id: id.into(),
                    delta: format!("{{\"command\":\"{command}\"}}"),
                });
            app.transcript
                .apply(&crate::events::OwnedEvent::ToolResult {
                    id: id.into(),
                    content: "ok".into(),
                    is_error: false,
                    code: Some(0),
                });
            // Models emit empty text parts between calls.
            app.transcript
                .apply(&crate::events::OwnedEvent::TextDelta("\n".into()));
        }

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let first = (0..buffer.area.height)
            .find(|y| row_text(*y).contains("$ git status"))
            .expect("first tool row visible");
        let second = (first..buffer.area.height)
            .find(|y| row_text(*y).contains("$ git log"))
            .expect("second tool row visible");
        // Tool lines sit tight: the model's empty text parts between
        // calls render as nothing.
        assert_eq!(second, first + 1, "no blank row between the calls");
    }

    /// Draws and returns the whole screen as one string.
    fn draw_screen(app: &mut App) -> String {
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_late_result_mark_rerenders_its_cached_block() {
        let mut app = chat_app();
        app.transcript.push_user("investigate");
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallStart {
                id: "c1".into(),
                name: "bash".into(),
            });
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolCallArgsDelta {
                id: "c1".into(),
                delta: "{\"command\":\"git status\"}".into(),
            });
        // A response behind the call: the result lands on a block that
        // is not the render's tail.
        app.transcript
            .apply(&crate::events::OwnedEvent::TextDelta("meanwhile".into()));
        let before = draw_screen(&mut app);
        assert!(before.contains("❯ investigate"), "{before}");
        assert!(before.contains("$ git status"), "{before}");
        assert!(!before.contains("[1]"), "unresolved: {before}");
        assert_eq!(app.transcript_cache.blocks.len(), 3);

        // The result mutates a line under the cache: the watermark must
        // retire the block, not serve the stale rows.
        app.transcript
            .apply(&crate::events::OwnedEvent::ToolResult {
                id: "c1".into(),
                content: "exit code 1".into(),
                is_error: true,
                code: Some(1),
            });
        let after = draw_screen(&mut app);
        assert!(after.contains("❯ investigate"), "{after}");
        assert!(after.contains("[1]"), "the mark lands: {after}");
    }

    #[test]
    fn a_markdown_block_renders_through_the_cache() {
        let mut app = chat_app();
        app.transcript
            .push_markdown("name | trigger\n--- | ---\nfmt | format code".to_string());
        let first = draw_screen(&mut app);
        assert!(
            first.contains("name") && first.contains("trigger"),
            "{first}"
        );
        assert!(first.contains("fmt"), "{first}");
        // A second frame draws from the cache and must not lose the block.
        let second = draw_screen(&mut app);
        assert_eq!(first, second, "the cached frame draws identically");
    }

    #[test]
    fn a_blank_row_separates_a_reply_from_the_next_prompt() {
        let mut app = chat_app();
        app.transcript.push_user("alpha");
        app.transcript
            .apply(&crate::events::OwnedEvent::TextDelta("beta reply".into()));
        app.transcript.push_user("gamma");

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let alpha_row = (0..buffer.area.height)
            .find(|y| row_text(*y).contains("alpha"))
            .expect("first prompt visible");
        let beta_row = (alpha_row..buffer.area.height)
            .find(|y| row_text(*y).contains("beta reply"))
            .expect("reply visible");
        let gamma_row = (beta_row..buffer.area.height)
            .find(|y| row_text(*y).contains("gamma"))
            .expect("second prompt visible");
        // Every block breathes: one blank row after the prompt, one
        // after the reply.
        assert_eq!(beta_row, alpha_row + 2, "a blank row after the prompt");
        assert_eq!(gamma_row, beta_row + 2, "one blank row after the reply");
    }
}
