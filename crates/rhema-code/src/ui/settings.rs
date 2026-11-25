//! The settings screen: the section sidebar and the page forms, with the
//! usage table and its k/M abbreviations.

use super::dialogs::{
    render_add_decision_dialog, render_add_dialog, render_add_mcp_dialog,
    render_add_profile_dialog, render_add_sandbox_dialog, render_add_websearch_dialog,
    render_instance_confirm, render_mcp_confirm,
};
use super::*;

/// The page's air: cells of padding on each side of its text, and rows
/// above its title, so a page reads as a page and not as a wall of text
/// against the frame. A short terminal gives the top row back: the
/// sidebar's sections must all fit before the air matters.
const PAGE_PAD: u16 = 3;
const PAGE_TOP: u16 = 2;
/// The height under which the top margin shrinks to one row.
const PAGE_TOP_MIN_HEIGHT: u16 = 22;

/// The settings screen: the sidebar of sections on the left, the selected
/// section's page on the right, its key hints pinned to the bottom row.
/// The settings screen: the sidebar of sections on the left, the
/// selected section's page, titled, ruled, padded, on the right, its
/// key hints pinned to the bottom row, and the add-instance dialog
/// popped over it all. Sidebar rows and interactive page rows register
/// click targets, the rects are what a click is matched against, so
/// only what was drawn is clickable.
pub(crate) fn render_settings(
    frame: &mut Frame,
    settings: &crate::screens::SettingsScreen,
    theme: &Theme,
    icons: &crate::icons::Icons,
    hits: &mut Vec<(Rect, SettingsHit)>,
) {
    let area = frame.area();
    let block = super::chrome::panel(None, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The sidebar holds its longest label, "Decision models", with
    // the marker and the padding: two cells of air inside its edge, one
    // against the page.
    let top = if inner.height >= PAGE_TOP_MIN_HEIGHT {
        PAGE_TOP
    } else {
        1
    };
    let [sidebar_area, page_area] =
        Layout::horizontal([Constraint::Length(20), Constraint::Min(24)]).areas(inner);
    // The page keeps its hints off the bottom edge, one row of air
    // above the border.
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(page_area);
    // The content pane wraps at its true text width, and the page sits
    // two rows under the border, three cells in from the sidebar.
    let text_width = content_area.width.saturating_sub(PAGE_PAD * 2) as usize;

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
                x: sidebar_area.x + 2,
                y: sidebar_area.y + top + 2 + index as u16,
                width: sidebar_area.width.saturating_sub(3),
                height: 1,
            },
            SettingsHit::Sidebar(*section),
        ));
    }
    frame.render_widget(
        Paragraph::new(sidebar).block(Block::default().padding(Padding::new(2, 1, top, 0))),
        sidebar_area,
    );

    let (page, footer, row_hits) = settings_page(settings, theme, text_width);
    // The Models page's rows are the shared picker's, drawn under the
    // page's heading: one cursor, the page's, over one list.
    let models_page = settings.section == SettingsSection::Models;
    // The page scrolls to its selected row: a short terminal keeps the
    // selection in view rather than clipping it.
    let viewport = content_area.height.saturating_sub(1);
    let scroll = if models_page {
        0
    } else {
        row_hits
            .iter()
            .find(|(_, hit)| *hit == SettingsHit::PageRow(settings.row))
            .map(|(line, _)| {
                u16::try_from(*line)
                    .unwrap_or(u16::MAX)
                    .saturating_add(1)
                    .saturating_sub(viewport)
            })
            .unwrap_or(0)
    };
    frame.render_widget(
        Paragraph::new(page)
            .scroll((scroll, 0))
            .block(Block::default().padding(Padding::new(PAGE_PAD, PAGE_PAD, top, 0))),
        content_area,
    );
    // The interactive rows sit at their rendered line indices, every
    // line the page builder produced is already wrapped to the pane's
    // text width, and the top padding offsets them one row down. Rows
    // the scroll pushed out of the pane take no clicks.
    for (line_index, hit) in row_hits {
        let line = u16::try_from(line_index).unwrap_or(u16::MAX);
        let Some(visible) = line.checked_sub(scroll) else {
            continue;
        };
        let y = content_area.y.saturating_add(top).saturating_add(visible);
        if y >= content_area.y.saturating_add(content_area.height) {
            continue;
        }
        hits.push((
            Rect {
                x: content_area.x + PAGE_PAD,
                y,
                width: text_width as u16,
                height: 1,
            },
            hit,
        ));
    }
    if models_page && content_area.height > top + 2 {
        // Under the header the page builder laid out: title, rule, one
        // row of air. The list sits on the page's own text column, so
        // its cursor lands where every other page's row cursor lands,
        // under the title, and its columns line up with the text around
        // them. The block takes the page's whole body whatever the
        // catalog holds: a filter box that slid up as rows were filtered
        // away would move out from under the hands typing in it.
        let header = top + 2;
        let room = content_area.height - header;
        let rows = super::model_picker::render_model_picker(
            frame,
            Rect {
                x: content_area.x + PAGE_PAD,
                y: content_area.y + header,
                width: text_width as u16,
                height: room,
            },
            &settings.model_picker,
            icons,
            theme,
            &super::model_picker::PickerChrome {
                title: None,
                // The page footer carries the hints; the picker's own
                // row stays the filter and the count.
                hints: "",
                focused: true,
                prompt: "",
            },
        );
        for (rect, index) in rows {
            hits.push((rect, SettingsHit::PageRow(index)));
        }
    }
    let footer = if models_page {
        // The page's keys follow the list's state: filter and pick once
        // the catalog settles, pick only while rows land, nothing but
        // the escape when there is no list at all.
        let hint = settings
            .model_picker
            .state_hints("type to filter · enter choose · ctrl-a all");
        vec![Span::raw(hint.to_owned()).fg(theme.text3)]
    } else {
        footer
    };
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::default().padding(Padding::horizontal(2))),
        footer_area,
    );

    let modal = settings.dialog.is_some()
        || settings.add_profile.is_some()
        || settings.sandbox_dialog.is_some()
        || settings.mcp_dialog.is_some()
        || settings.mcp_confirm.is_some()
        || settings.websearch_confirm.is_some()
        || settings.decision_confirm.is_some()
        || settings.instance_confirm.is_some();
    if modal {
        super::chrome::dim_backdrop(frame.buffer_mut(), theme);
    }
    if let Some(dialog) = &settings.dialog {
        render_add_dialog(frame, dialog, theme, hits);
    }
    if let Some(dialog) = &settings.add_profile {
        render_add_profile_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.sandbox_dialog {
        render_add_sandbox_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.websearch_dialog {
        render_add_websearch_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.decision_dialog {
        render_add_decision_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.mcp_dialog {
        render_add_mcp_dialog(frame, dialog, theme);
    }
    if let Some(name) = &settings.mcp_confirm {
        render_mcp_confirm(frame, name, theme);
    }
    if let Some(name) = &settings.websearch_confirm {
        render_instance_confirm(frame, name, theme);
    }
    if let Some(name) = &settings.decision_confirm {
        render_instance_confirm(frame, name, theme);
    }
    if let Some(name) = &settings.instance_confirm {
        render_instance_confirm(frame, name, theme);
    }
}

/// Tokens as the Usage page spells an amount: kilo-scaled past a
/// thousand and mega-scaled past a million, one decimal, the trailing
/// `.0` trimmed.
pub(crate) fn usage_k(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        trim_tenth(format!("{:.1}M", tokens as f64 / 1_000_000.0), "M")
    } else if tokens >= 1_000 {
        trim_tenth(format!("{:.1}k", tokens as f64 / 1_000.0), "k")
    } else {
        tokens.to_string()
    }
}

/// Cuts a scaled amount's trailing `.0`: `12.0k` reads `12k`.
pub(crate) fn trim_tenth(amount: String, suffix: &str) -> String {
    match amount.strip_suffix(&format!(".0{suffix}")) {
        Some(base) => format!("{base}{suffix}"),
        None => amount,
    }
}

/// One `label  value` row of a settings page; the label column reads
/// dim. Wrapped to the pane, so the rendered line count is the
/// builder's to know.
pub(crate) fn settings_field(
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
pub(crate) fn settings_hint(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    wrap_line(
        &Line::from(Span::raw(text.to_owned()).fg(theme.text3)),
        width,
    )
}

/// A selectable page row: the ❯ marker, the line lit when selected.
pub(crate) fn settings_row(line: Line<'static>, selected: bool, theme: &Theme) -> Line<'static> {
    if selected {
        line.style(Style::default().bg(theme.elevated))
    } else {
        line
    }
}

/// The view's shape: a bar of the block's estimated tokens against the
/// band's, then the cut and the band as their own rows. The block is
/// drawn at least one cell wide, so one that is a rounding error still
/// reads as present; the percentage is what says it is small.
fn view_shape(
    stats: &rhema_transcript::TranscriptStats,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    /// The bar's width in cells: a sliver stays visible, and the line
    /// still fits the page at its usual 80 columns.
    const CELLS: usize = 24;
    if stats.requests == 0 {
        return settings_field(
            "View",
            Span::raw("nothing measured yet".to_owned()).fg(theme.text3),
            theme,
            width,
        );
    }
    if stats.block_bytes == 0 {
        return settings_field(
            "View",
            Span::raw(format!(
                "under budget · {} requests rode the log whole",
                stats.pass_through
            ))
            .fg(theme.text3),
            theme,
            width,
        );
    }
    // One cell, at least, for a block that rounds to nothing: the bar
    // says the block is present, the percentage says it is small.
    let total = (stats.block_tokens + stats.band_tokens).max(1);
    let filled = ((CELLS as u64 * stats.block_tokens + total / 2) / total) as usize;
    let block_cells = if stats.block_tokens == 0 {
        0
    } else {
        filled.clamp(1, CELLS - 1)
    };
    let share = stats.block_tokens as f64 / total as f64 * 100.0;
    let bar = Line::from(vec![
        Span::raw(format!("{:<18}", "View")).fg(theme.text3),
        Span::raw("\u{2588}".repeat(block_cells)).fg(theme.accent),
        Span::raw("\u{2591}".repeat(CELLS - block_cells)).fg(theme.border_strong),
        Span::raw(format!("  {share:.1}% block")).fg(theme.text3),
    ]);
    let mut lines = wrap_line(&bar, width);
    lines.extend(settings_field(
        "Cut",
        Span::raw(format!(
            "{} of {} messages · {} digests",
            stats.sectioned_messages, stats.log_messages, stats.sections
        ))
        .fg(theme.text),
        theme,
        width,
    ));
    // The band's whole images; zero shows nothing, the line stays
    // about bytes then.
    let images = if stats.view_images > 0 {
        format!(" · {} images", stats.view_images)
    } else {
        String::new()
    };
    lines.extend(settings_field(
        "Band",
        Span::raw(format!(
            "~{} carried whole · ~{} this turn{}",
            usage_k(stats.band_tokens),
            usage_k(stats.tail_tokens),
            images,
        ))
        .fg(theme.text),
        theme,
        width,
    ));
    lines
}

/// A settings page: its content lines, its footer hints, and the click
/// targets of its interactive rows as rendered line indices, the text
/// is wrapped to the pane's width, so the indices are exact.
pub(crate) fn settings_page(
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
        Line::from(super::chrome::rule(width, theme)),
        Line::default(),
    ];
    match settings.section {
        // The picker draws this page's rows under the heading.
        SettingsSection::Models => (lines, Vec::new(), Vec::new()),
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
        SettingsSection::Profiles => {
            let mut row_hits = Vec::new();
            for (index, row) in settings.profiles.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The dot wears the profile's color, the stored hex,
                // else the theme's tone for the name.
                let color = row
                    .color
                    .as_deref()
                    .and_then(crate::palette::parse_hex_color)
                    .unwrap_or_else(|| crate::palette::default_profile_color(&row.name, theme));
                let mut detail = String::new();
                match &row.provider {
                    Some(provider) => detail.push_str(provider),
                    None => detail.push_str("no provider"),
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
                let line = Line::from(vec![
                    Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                    Span::raw("● ").fg(color),
                    Span::raw(row.name.clone()).fg(color),
                    Span::raw(format!("  {detail}")).fg(theme.text3),
                ]);
                lines.push(settings_row(line, selected, theme));
            }
            let add = settings.profiles.len();
            let selected = settings.row == add;
            row_hits.push((lines.len(), SettingsHit::PageRow(add)));
            lines.push(settings_row(
                Line::from(vec![
                    Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                    Span::raw("+ Add a profile...").fg(theme.text2),
                ]),
                selected,
                theme,
            ));
            lines.push(Line::default());
            (lines, footer("enter switch · c color · d remove"), row_hits)
        }
        SettingsSection::Sandbox => {
            let mut row_hits = Vec::new();
            for (index, row) in settings.sandbox_rows().iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                let line = if row.add {
                    Line::from(vec![
                        Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                        Span::raw("+ Add a sandbox instance...").fg(theme.text2),
                    ])
                } else {
                    let mut detail = if row.name == "local" {
                        row.detail.clone()
                    } else {
                        format!("{} · {}", row.kind.stored_name(), row.detail)
                    };
                    if row.active {
                        detail.push_str(" · active");
                    }
                    if row.remembered {
                        detail.push_str(" · last used here");
                    }
                    if !row.offered {
                        detail.push_str(" · not offered here");
                    }
                    let name_style = if row.active {
                        Style::default().fg(theme.text).bold()
                    } else {
                        Style::default().fg(theme.text)
                    };
                    Line::from(vec![
                        Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                        Span::styled(row.name.to_owned(), name_style),
                        Span::raw(format!("  {detail}")).fg(theme.text3),
                    ])
                };
                lines.push(settings_row(line, selected, theme));
            }
            lines.push(Line::default());
            (
                lines,
                footer(
                    "enter runs bash here · d removes the instance · docker and lxd can be added",
                ),
                row_hits,
            )
        }
        SettingsSection::Appearance => {
            let mut row_hits = Vec::new();
            let toggles = [
                ("Colors", settings.colors.label()),
                ("Theme", if settings.theme_dark { "Dark" } else { "Light" }),
                (
                    "Background",
                    if settings.background_native {
                        "Native"
                    } else {
                        "Rhema"
                    },
                ),
                (
                    "Icons",
                    if settings.icons_nerd {
                        "Nerd"
                    } else {
                        "Unicode"
                    },
                ),
                ("Cursor", settings.cursor.label()),
                ("Scrollbar", if settings.scrollbar { "On" } else { "Off" }),
            ];
            for (index, (label, value)) in toggles.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                let line = Line::from(vec![
                    Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text),
                    Span::raw(format!("{:<11}", label)).fg(theme.text3),
                    Span::raw((*value).to_owned()).fg(theme.text),
                ]);
                lines.push(settings_row(line, selected, theme));
            }
            lines.push(Line::default());
            (
                lines,
                footer("c colors · t theme · b background · i icons · s cursor · r scrollbar"),
                row_hits,
            )
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
            let mut row_hits = Vec::new();
            if settings.skills.is_empty() {
                lines.extend(settings_hint("No skills discovered", theme, width));
            }
            for (index, skill) in settings.skills.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The dot is the on/off state: filled and ok-green on,
                // hollow and dim off, the same pair the skills step of
                // the onboarding wizard draws.
                let dot = if skill.enabled { "●" } else { "○" };
                let dot_color = if skill.enabled { theme.ok } else { theme.text3 };
                let line = Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw(dot).fg(dot_color),
                    Span::raw(" ").fg(theme.text3),
                    Span::raw(skill.name.clone()).fg(if skill.enabled {
                        theme.text
                    } else {
                        theme.text3
                    }),
                    Span::raw(format!(" · {}", skill.source.label())).fg(theme.text3),
                ]);
                lines.push(settings_row(line, selected, theme));
                // The description, at most two rows, the same cap the
                // Subagents page applies, so both catalogs scan alike.
                lines.extend(crate::ui::wrap_clamped(
                    &skill.description,
                    width,
                    4,
                    2,
                    Style::default().fg(theme.text3),
                ));
                // The license sits a step dimmer than the description:
                // same tone, dim modifier, attribution that reads only
                // when looked for, and never cut: a clipped copyright
                // line fails its one job.
                if let Some(license) = &skill.license {
                    lines.extend(crate::ui::wrap_clamped(
                        license,
                        width,
                        4,
                        2,
                        Style::default().fg(theme.text3).dim(),
                    ));
                }
            }
            (lines, footer("enter/space - toggle on and off"), row_hits)
        }
        SettingsSection::Subagents => {
            lines.extend(settings_field(
                "Global",
                Span::raw(
                    settings
                        .subagents_global_dir
                        .clone()
                        .unwrap_or_else(|| "None".into()),
                )
                .fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field(
                "Workspace",
                Span::raw(settings.subagents_workspace_dir.clone()).fg(theme.text),
                theme,
                width,
            ));
            lines.push(Line::default());
            if settings.subagents.is_empty() {
                lines.extend(settings_hint("No subagents discovered", theme, width));
            }
            let mut row_hits = Vec::new();
            for (index, subagent) in settings.subagents.iter().enumerate() {
                let selected = index == settings.row;
                // The hit rect lands on the name line; the detail lines
                // under it are description, not a second target.
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                let marker = Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text);
                // The dot is the on/off state: filled and ok-green on,
                // hollow and dim off, the Skills page's pair.
                let dot = if subagent.enabled { "●" } else { "○" };
                let dot_color = if subagent.enabled {
                    theme.ok
                } else {
                    theme.text3
                };
                let line = Line::from(vec![
                    marker,
                    Span::raw(dot).fg(dot_color),
                    Span::raw(" ").fg(theme.text3),
                    Span::raw(subagent.name.clone()).fg(if subagent.enabled {
                        theme.text
                    } else {
                        theme.text3
                    }),
                    Span::raw(format!(" · {}", subagent.source.label())).fg(theme.text3),
                ]);
                lines.push(settings_row(line, selected, theme));
                // The description, at most two rows: prose, indented
                // like the name, an ellipsis when it does not fit.
                lines.extend(crate::ui::wrap_clamped(
                    &subagent.description,
                    width,
                    4,
                    2,
                    Style::default().fg(theme.text3),
                ));
                // Overrides only: a definition that changes nothing
                // renders no meta line, the session's model and the
                // default step budget are the norm, and a number that
                // looks configured when it is a default reads wrong.
                let mut meta = Vec::new();
                if let Some(model) = &subagent.model {
                    meta.push(model.clone());
                }
                if let Some(steps) = subagent.max_steps {
                    meta.push(format!("{steps} steps max"));
                }
                if !meta.is_empty() {
                    lines.extend(wrap_line(
                        &Line::from(Span::raw(format!("    {}", meta.join(" · "))).fg(theme.text3)),
                        width,
                    ));
                }
            }
            // The hints must not clip on a narrow terminal: the edit
            // note rides only when the pane fits it whole.
            let mut hints = "space - toggle on and off · enter/e - edit in $EDITOR".to_owned();
            let note = "; the built-in is copied to the global dir first";
            if width >= hints.chars().count() + note.chars().count() + 2 {
                hints.push_str(note);
            }
            (lines, footer(&hints), row_hits)
        }
        SettingsSection::Mcp => {
            if settings.mcp.is_empty() {
                lines.extend(settings_hint("no servers in this profile", theme, width));
                lines.extend(settings_hint(
                    "the add row below installs one",
                    theme,
                    width,
                ));
            }
            let mut row_hits = Vec::new();
            for (index, server) in settings.mcp.iter().enumerate() {
                let selected = index == settings.row;
                // The hit rect lands on the name line; the description
                // and notice lines under it are detail, not a second
                // target.
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The dot is the muted state: filled for a server the
                // manager starts, hollow and dim for one the session
                // mutes, the skills page's pair, meanings swapped.
                let dot = if server.disabled { "○" } else { "●" };
                let dot_color = if server.disabled {
                    theme.text3
                } else {
                    theme.ok
                };
                let name_color = if server.disabled {
                    theme.text3
                } else {
                    theme.text
                };
                let mut line = Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw(dot).fg(dot_color),
                    Span::raw(" ").fg(theme.text3),
                    Span::raw(server.name.clone()).fg(name_color),
                ]);
                if let Some(transport) = server.transport {
                    line.push_span(Span::raw(format!(" · {transport}")).fg(theme.text3));
                } else {
                    // No marker: the entry resolves to neither
                    // transport, and the notice under the row says
                    // what the assembly does about it.
                    line.push_span(Span::raw(" · unresolvable").fg(theme.warn));
                }
                if server.disabled {
                    line.push_span(Span::raw(" · muted").fg(theme.text3));
                }
                lines.push(settings_row(line, selected, theme));
                lines.extend(wrap_line(
                    &Line::from(Span::raw(format!("    {}", server.description))).fg(theme.text3),
                    width,
                ));
                if let Some(notice) = &server.notice {
                    lines.extend(wrap_line(
                        &Line::from(Span::raw(format!("    {notice}"))).fg(theme.warn),
                        width,
                    ));
                }
            }
            let add = settings.mcp.len();
            let selected = settings.row == add;
            row_hits.push((lines.len(), SettingsHit::PageRow(add)));
            lines.push(settings_row(
                Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw("+ Add a server...").fg(theme.text2),
                ]),
                selected,
                theme,
            ));
            lines.push(Line::default());
            (
                lines,
                footer("enter mutes · d removes · the add row installs one"),
                row_hits,
            )
        }
        SettingsSection::WebSearch => {
            // The rows always carry their add row; empty means no
            // provider above it.
            if settings.websearch.iter().all(|row| row.add) {
                lines.extend(settings_hint("no providers in this profile", theme, width));
                lines.extend(settings_hint(
                    "the add row below installs one, or export a BRAVE_SEARCH_API_KEY / \
                     EXA_API_KEY / TAVILY_API_KEY",
                    theme,
                    width,
                ));
            }
            let mut row_hits = Vec::new();
            for (index, row) in settings.websearch.iter().enumerate() {
                // The add row is the sentinel closing the vec; the
                // hand-built line below renders it, the loop draws
                // providers only.
                if row.add {
                    continue;
                }
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The position is the chain's truth: `search` walks
                // 1.., first success wins. An unordered row lists
                // dim, inert, and `[` enrolls it.
                let dim = !row.ordered;
                let line = Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw(match row.position {
                        Some(position) => format!("{position}."),
                        None => "\u{b7}".to_owned(),
                    })
                    .fg(if dim { theme.text3 } else { theme.text2 }),
                    Span::raw(" ").fg(theme.text3),
                    Span::raw(row.name.clone()).fg(if dim { theme.text3 } else { theme.text }),
                    Span::raw(format!(" \u{b7} {}", row.kind)).fg(theme.text3),
                ]);
                lines.push(settings_row(line, selected, theme));
            }
            // The add row is the sentinel closing the vec: its index
            // is the last, not the len — a len index is out of range,
            // and the click would land nowhere.
            let add = settings.websearch.len().saturating_sub(1);
            let selected = settings.row == add;
            row_hits.push((lines.len(), SettingsHit::PageRow(add)));
            lines.push(settings_row(
                Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw("+ Add a provider...").fg(theme.text2),
                ]),
                selected,
                theme,
            ));
            lines.push(Line::default());
            (
                lines,
                footer("k up \u{b7} j down \u{b7} d removes \u{b7} the add row installs one"),
                row_hits,
            )
        }
        SettingsSection::Decision => {
            if settings.decision.iter().all(|row| row.add) {
                lines.extend(settings_hint(
                    "no decision models in this profile",
                    theme,
                    width,
                ));
            }
            let mut row_hits = Vec::new();
            for (index, row) in settings.decision.iter().enumerate() {
                // The add row is the sentinel closing the vec; the
                // hand-built line below renders it, the loop draws
                // instances only.
                if row.add {
                    continue;
                }
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The kind shows as stored, the file's spelling, the
                // web search rows' rule.
                let mut detail = format!("{} · {}", row.kind, row.model);
                if let Some(url) = &row.base_url {
                    detail.push_str(&format!(" · {url}"));
                }
                if !row.keyed {
                    detail.push_str(" · no key");
                }
                if row.default {
                    detail.push_str(" · default");
                }
                let line = Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw(row.name.clone()).fg(theme.text),
                    Span::raw(format!(" · {detail}")).fg(theme.text3),
                ]);
                // An endpoint makes a row longer than a narrow pane:
                // it wraps, and the first line carries the click.
                lines.extend(
                    wrap_line(&line, width)
                        .into_iter()
                        .enumerate()
                        .map(|(index, line)| settings_row(line, selected && index == 0, theme)),
                );
            }
            let add = settings.decision.len().saturating_sub(1);
            let selected = settings.row == add;
            row_hits.push((lines.len(), SettingsHit::PageRow(add)));
            lines.push(settings_row(
                Line::from(vec![
                    super::chrome::marker(selected, theme),
                    Span::raw("+ Add a decision model...").fg(theme.text2),
                ]),
                selected,
                theme,
            ));
            lines.push(Line::default());
            (
                lines,
                footer("enter sets the default · d removes"),
                row_hits,
            )
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
                    std::iter::once(Span::raw(format!("{:<12}", "Allowance")).fg(theme.text3))
                        .chain(allowance_value)
                        .collect::<Vec<_>>(),
                ),
                width,
            ));
            lines.extend(settings_field(
                "Session",
                Span::raw(cost).fg(theme.text),
                theme,
                width,
            ));
            lines.extend(settings_field("Last Turn", last_turn, theme, width));
            // The compaction benchmark, when the transcript is on: the
            // view's shape first, so the counters below have something
            // to point at.
            if let Some(stats) = settings.transcript {
                lines.extend(view_shape(&stats, theme, width));
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
                    "Cache",
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
                    "{}/{} requests viewed, {} flushes, block ~{} tokens, {}",
                    stats.viewed,
                    stats.requests,
                    stats.flushes,
                    usage_k(stats.block_tokens),
                    worker,
                );
                lines.extend(settings_field(
                    "Compaction",
                    Span::raw(compaction).fg(theme.text),
                    theme,
                    width,
                ));
            }
            // The detailed split: what the session's tokens bought,
            // each as its share of the total and in three counts. The
            // history rides every request, so a category's billed sum
            // dwarfs the content it stands for, and the log holds that
            // content once; the row without the other two reads as if
            // the session held millions of tokens of tool output.
            lines.push(Line::default());
            lines.push(Line::from(Span::raw("Detailed Session Usage")).bold());
            match settings.transcript {
                Some(stats) => {
                    let cache = match stats.cache_hit_rate() {
                        Some(rate) => format!("{:.0}% cached", rate * 100.0),
                        None => "no usage reported yet".to_owned(),
                    };
                    lines.extend(settings_hint(
                        &format!(
                            "sent is what one request carried on average, log is the \
                             conversation's own content counted once at about four bytes to \
                             the token, billed is the provider's sum over {} requests; {}",
                            stats.requests, cache,
                        ),
                        theme,
                        width,
                    ));
                    // No requests, no average: the rows would all read
                    // zero, which is what they say anyway.
                    let requests = stats.requests.max(1);
                    let billed = stats.usage.total();
                    lines.extend(settings_field(
                        "Total",
                        Span::raw(format!(
                            "{} sent \u{b7} ~{} log \u{b7} {} billed",
                            usage_k(billed / requests),
                            usage_k(stats.log_content.total()),
                            usage_k(billed),
                        ))
                        .fg(theme.text),
                        theme,
                        width,
                    ));
                    // The log figures line up with the breakdown's row
                    // order. The system prompt and the skills ride
                    // every request but live in no message, so the log
                    // holds none of them.
                    let of_log = [
                        None,
                        None,
                        Some(stats.log_content.tool_calls),
                        Some(stats.log_content.tool_results),
                        Some(stats.log_content.user_text),
                        Some(stats.log_content.assistant_text),
                    ];
                    for ((label, tokens), once) in stats.usage.labeled().into_iter().zip(of_log) {
                        let pct = if billed > 0 {
                            tokens as f64 / billed as f64 * 100.0
                        } else {
                            0.0
                        };
                        let once = match once {
                            Some(log) => format!("~{} log", usage_k(log)),
                            None => "no log".to_owned(),
                        };
                        let value = format!(
                            "{pct:.0}% {} sent \u{b7} {once} \u{b7} {} billed",
                            usage_k(tokens / requests),
                            usage_k(tokens),
                        );
                        lines.extend(settings_field(
                            label,
                            Span::raw(value).fg(theme.text),
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
        SettingsSection::Update => {
            let mut row_hits = Vec::new();
            let current = crate::update::current_version();
            lines.extend(settings_field(
                "Current",
                Span::raw(current.to_owned()).fg(theme.text),
                theme,
                width,
            ));
            let latest = match &settings.update.check {
                crate::screens::UpdateCheck::Idle => {
                    Span::raw("not checked yet".to_owned()).fg(theme.text3)
                }
                crate::screens::UpdateCheck::Checking => {
                    Span::raw("checking…".to_owned()).fg(theme.text3)
                }
                crate::screens::UpdateCheck::Done(Ok(latest)) => {
                    // The verdict rides the version: the channel's
                    // number beside this build's, colored by what a
                    // restart would bring.
                    let newer = crate::update::is_newer(latest, current);
                    Span::raw(format!(
                        "{latest}, {}",
                        if newer {
                            "update available"
                        } else {
                            "up to date"
                        }
                    ))
                    .fg(if newer { theme.warn } else { theme.ok })
                }
                crate::screens::UpdateCheck::Done(Err(err)) => {
                    Span::raw(err.clone()).fg(theme.warn)
                }
            };
            lines.extend(settings_field("Latest", latest, theme, width));
            lines.push(Line::default());
            // The two rows. The install row lists dim until a check
            // earned it, a row that would refuse every enter must
            // say so before the enter.
            let rows = [
                ("Check for updates...", true),
                ("Update now", settings.update.update_available()),
            ];
            for (index, (label, enabled)) in rows.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                lines.push(settings_row(
                    Line::from(vec![
                        super::chrome::marker(selected, theme),
                        Span::raw((*label).to_owned()).fg(if *enabled {
                            theme.text2
                        } else {
                            theme.text3
                        }),
                    ]),
                    selected,
                    theme,
                ));
            }
            lines.push(Line::default());
            // The install run's outcome, when there is one: the
            // script's last lines (its final line names what landed
            // where), or why the run died.
            match &settings.update.install {
                crate::screens::UpdateInstall::Idle => {}
                crate::screens::UpdateInstall::Running => lines.extend(settings_hint(
                    "downloading and installing, this can take a minute",
                    theme,
                    width,
                )),
                crate::screens::UpdateInstall::Done(Ok(output)) => {
                    let tail: Vec<&str> = output.lines().rev().take(3).collect();
                    for line in tail.into_iter().rev() {
                        lines.extend(settings_hint(line, theme, width));
                    }
                    lines.extend(settings_hint(
                        "a restart picks the new version up",
                        theme,
                        width,
                    ));
                }
                crate::screens::UpdateInstall::Done(Err(err)) => {
                    lines.extend(settings_hint(
                        &format!("the install failed: {err}"),
                        theme,
                        width,
                    ));
                }
            }
            (
                lines,
                footer("enter checks or installs · the install runs in the background"),
                row_hits,
            )
        }
        SettingsSection::About => {
            lines.push(Line::from(
                Span::raw(format!("Rhema Code {}", env!("CARGO_PKG_VERSION"))).bold(),
            ));
            lines.extend(settings_field(
                "Website",
                Span::raw("https://rhema.rs".to_owned()).fg(theme.text),
                theme,
                width,
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
