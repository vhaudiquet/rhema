//! The dialogs: the add flows, the model and resume pickers, and the two
//! modals — each a centered box drawn over the current screen.

use super::*;

/// The add-instance dialog: a pop-in box over the Providers page with
/// the provider list, the name and credential inputs, or the running
/// flow. The provider rows register click targets.
pub(crate) fn render_add_dialog(
    frame: &mut Frame,
    dialog: &crate::screens::AddInstanceDialog,
    theme: &Theme,
    hits: &mut Vec<(Rect, SettingsHit)>,
) {
    // The inner width of the centered box: the text lines wrap to it.
    const INNER_WIDTH: usize = 56;
    let flow = dialog_flow_lines(dialog, theme, INNER_WIDTH);
    let rows = match dialog.stage {
        crate::screens::DialogStage::SelectCategory => ProviderCategory::ALL.len(),
        crate::screens::DialogStage::SelectProvider => dialog.visible_providers().len(),
        crate::screens::DialogStage::NameInstance
        | crate::screens::DialogStage::EnterCredential => 1,
        crate::screens::DialogStage::Flow => flow.lines.len().max(1),
    } as u16;
    let height = 2 + rows + 1 + 1;
    let area = centered(58, height, frame.area());
    frame.render_widget(Clear, area);
    let heading = match dialog.stage {
        crate::screens::DialogStage::SelectProvider => {
            format!(" Add a login · {} ", dialog.selected_category().label())
        }
        _ => " Add a login ".to_owned(),
    };
    // The dialog is the screen's focus: the frame takes the readable
    // ink rather than the dim hairline, and the title its own bright
    // ink instead of the border's fg.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.text3))
        .title_style(Style::default().fg(theme.text).bold())
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(heading);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, _input_area, status_area] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    match dialog.stage {
        crate::screens::DialogStage::SelectCategory => {
            // The groups a provider can belong to, each row a click
            // target; enter (or a click) opens the group's list.
            let rows: Vec<ListItem> = ProviderCategory::ALL
                .iter()
                .enumerate()
                .map(|(index, category)| {
                    let selected = index == dialog.selected_category;
                    hits.push((
                        Rect {
                            x: body_area.x,
                            y: body_area.y + index as u16,
                            width: body_area.width,
                            height: 1,
                        },
                        SettingsHit::DialogCategory(index),
                    ));
                    let name_style = if selected {
                        Style::default().fg(theme.text).bold()
                    } else {
                        Style::default().fg(theme.text2)
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                        Span::styled(category.label(), name_style),
                    ]))
                })
                .collect();
            frame.render_widget(ratatui::widgets::List::new(rows), body_area);
        }
        crate::screens::DialogStage::SelectProvider => {
            // The provider kinds a new instance can be, each row a click
            // target; enter (or a second click's enter) confirms.
            let rows: Vec<ListItem> = dialog
                .visible_providers()
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
            frame.render_widget(Paragraph::new(flow.lines), body_area);
            if let Some(row) = flow.buttons_row {
                let y = body_area.y + row as u16;
                let open_width = FLOW_OPEN_BUTTON.chars().count() as u16;
                let copy_width = FLOW_COPY_BUTTON.chars().count() as u16;
                hits.push((
                    Rect {
                        x: body_area.x,
                        y,
                        width: open_width,
                        height: 1,
                    },
                    SettingsHit::DialogFlowOpen,
                ));
                hits.push((
                    Rect {
                        x: body_area.x + open_width + FLOW_BUTTON_GAP,
                        y,
                        width: copy_width,
                        height: 1,
                    },
                    SettingsHit::DialogFlowCopy,
                ));
            }
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

/// The add-profile dialog: one input line over its status line, the
/// Profiles page's pop-over for naming a new profile.
pub(crate) fn render_add_profile_dialog(
    frame: &mut Frame,
    dialog: &crate::screens::AddProfileDialog,
    theme: &Theme,
) {
    let area = centered(58, 4, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" add a profile ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, status_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("name: ").fg(theme.text3),
            Span::raw(dialog.input.clone()).bold(),
        ])),
        body_area,
    );
    let status = match &dialog.error {
        Some(error) => Line::from(error.clone()).fg(theme.error),
        None => Line::from("name the profile · enter creates · esc cancels").fg(theme.text3),
    };
    frame.render_widget(Paragraph::new(status), status_area);
}

/// The add-server dialog: the stage's input line, the transport list,
/// or the lines editor, over its status line — the MCP page's pop-in
/// for installing a server.
pub(crate) fn render_add_mcp_dialog(
    frame: &mut Frame,
    dialog: &crate::screens::AddMcpServerDialog,
    theme: &Theme,
) {
    use crate::screens::McpDialogStage;
    let rows = match dialog.stage {
        McpDialogStage::Transport => crate::screens::MCP_TRANSPORTS.len(),
        McpDialogStage::EnvLines => dialog.env.lines.len(),
        McpDialogStage::HeaderLines => dialog.headers.lines.len(),
        _ => 1,
    } as u16;
    let area = centered(58, rows + 3, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" add an MCP server ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, status_area] =
        Layout::vertical([Constraint::Length(rows), Constraint::Length(1)]).areas(inner);

    match dialog.stage {
        McpDialogStage::Transport => {
            let rows: Vec<Line> = crate::screens::MCP_TRANSPORTS
                .iter()
                .enumerate()
                .map(|(index, (name, what))| {
                    let selected = index == dialog.selected;
                    let name_style = if selected {
                        Style::default().fg(theme.text).bold()
                    } else {
                        Style::default().fg(theme.text2)
                    };
                    Line::from(vec![
                        Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                        Span::styled((*name).to_owned(), name_style),
                        Span::raw(format!("  {what}")).fg(theme.text3),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(rows), body_area);
        }
        McpDialogStage::EnvLines | McpDialogStage::HeaderLines => {
            let lines = match dialog.stage {
                McpDialogStage::EnvLines => &dialog.env,
                _ => &dialog.headers,
            };
            let rows: Vec<Line> = lines
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    // The cursor line wears the caret: the other
                    // dialogs' input line is always the only line, so
                    // this stage needs the mark to say where typing
                    // lands.
                    let caret = if index == lines.cursor { "▌" } else { "" };
                    Line::from(vec![
                        Span::raw(line.clone()).fg(if index == lines.cursor {
                            theme.text
                        } else {
                            theme.text2
                        }),
                        Span::raw(caret).fg(theme.text),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(rows), body_area);
        }
        _ => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(dialog.input_label()).fg(theme.text3),
                    Span::raw(dialog.input.clone()).bold(),
                ])),
                body_area,
            );
        }
    }

    let status = match &dialog.error {
        Some(error) => Line::from(error.clone()).fg(theme.error),
        None => Line::from(dialog.input_hint()).fg(theme.text3),
    };
    frame.render_widget(Paragraph::new(status), status_area);
}

/// The removal confirm: one small box asking about the one server, in
/// the quit confirm's shape — `y` answers, anything else leaves the
/// row alone.
pub(crate) fn render_mcp_confirm(frame: &mut Frame, name: &str, theme: &Theme) {
    let width = (name.chars().count() + 24).max(34) as u16;
    let area = centered(width, 3, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_strong))
        .style(Style::default().bg(theme.surface).fg(theme.text))
        .title(format!(" remove the {name} server? "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("y").bold(),
            Span::raw(" remove · "),
            Span::raw("n").bold(),
            Span::raw(" keep"),
        ])),
        inner,
    );
}

pub(crate) fn render_add_sandbox_dialog(
    frame: &mut Frame,
    dialog: &crate::screens::AddSandboxDialog,
    theme: &Theme,
) {
    let body: u16 = 2 + crate::screens::SANDBOX_ADD_KINDS.len() as u16;
    let area = centered(58, body + 2, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken).fg(theme.text))
        .title(" add a sandbox instance ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body_area, status_area] =
        Layout::vertical([Constraint::Length(body), Constraint::Length(1)]).areas(inner);
    let rows: Vec<Line> = crate::screens::SANDBOX_ADD_KINDS
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            let selected = index == dialog.selected;
            let mark = if selected { "❯ " } else { "  " };
            let what = match kind {
                crate::args::SandboxChoice::Docker => "a container, named after this workspace",
                crate::args::SandboxChoice::Lxd => "an LXD instance, named after this workspace",
                crate::args::SandboxChoice::Ecrou => {
                    "a microVM per command, named after this workspace"
                }
                crate::args::SandboxChoice::Ssh => "a remote box, by user@host",
                crate::args::SandboxChoice::Http => "a rhema-sandboxd, by url",
                crate::args::SandboxChoice::Local => "this host",
            };
            Line::from(vec![
                Span::raw(mark).fg(theme.accent_text),
                Span::raw(kind.stored_name().to_owned()).fg(theme.text),
                Span::raw(format!("  {what}")).fg(theme.text3),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), body_area);
    frame.render_widget(
        Paragraph::new("arrows choose · enter stores · esc cancels").fg(theme.text3),
        status_area,
    );
}

pub(crate) fn render_picker(
    frame: &mut Frame,
    picker: &PickerScreen,
    icons: &crate::icons::Icons,
    theme: &Theme,
) {
    let area = frame.area();
    let block = super::chrome::panel(Some("select a model"), theme);
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
        .map(|entry| ListItem::new(model_row(entry.provider, &entry.model, icons, theme)))
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
pub(crate) fn render_profile_picker(
    frame: &mut Frame,
    screen: &crate::screens::ProfileScreen,
    theme: &Theme,
) {
    let area = frame.area();
    let block = super::chrome::panel(Some("select a profile"), theme);
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
pub(crate) fn render_resume_picker(
    frame: &mut Frame,
    screen: &crate::screens::ResumeScreen,
    theme: &Theme,
) {
    let area = frame.area();
    let block = super::chrome::panel(Some("resume a session"), theme);
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

pub(crate) fn render_approval_modal(frame: &mut Frame, call: &rhema_core::ToolCall, theme: &Theme) {
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

pub(crate) fn render_confirm_quit(frame: &mut Frame, theme: &Theme) {
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
