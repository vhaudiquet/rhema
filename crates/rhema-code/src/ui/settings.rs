//! The settings screen: the section sidebar and the page forms, with the
//! usage table and its k/M abbreviations.

use super::dialogs::{
    render_add_dialog, render_add_mcp_dialog, render_add_profile_dialog, render_add_sandbox_dialog,
    render_mcp_confirm,
};
use super::*;

/// The settings screen: the sidebar of sections on the left, the selected
/// section's page on the right, its key hints pinned to the bottom row.
/// The settings screen: the sidebar of sections on the left, the
/// selected section's page — titled, ruled, padded — on the right, its
/// key hints pinned to the bottom row, and the add-instance dialog
/// popped over it all. Sidebar rows and interactive page rows register
/// click targets — the rects are what a click is matched against, so
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
    if let Some(dialog) = &settings.add_profile {
        render_add_profile_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.sandbox_dialog {
        render_add_sandbox_dialog(frame, dialog, theme);
    }
    if let Some(dialog) = &settings.mcp_dialog {
        render_add_mcp_dialog(frame, dialog, theme);
    }
    if let Some(name) = &settings.mcp_confirm {
        render_mcp_confirm(frame, name, theme);
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

/// A settings page: its content lines, its footer hints, and the click
/// targets of its interactive rows as rendered line indices — the text
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
                // The dot wears the profile's color — the stored hex,
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
                footer("c - colors, t - theme, b - background, i - icons"),
                row_hits,
            )
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
            let mut row_hits = Vec::new();
            if settings.skills.is_empty() {
                lines.extend(settings_hint("No skills discovered", theme, width));
            }
            for (index, skill) in settings.skills.iter().enumerate() {
                let selected = index == settings.row;
                row_hits.push((lines.len(), SettingsHit::PageRow(index)));
                // The dot is the on/off state: filled and ok-green on,
                // hollow and dim off — the same pair the skills step of
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
                lines.extend(wrap_line(
                    &Line::from(Span::raw(format!("    {}", skill.description)).fg(theme.text3)),
                    width,
                ));
                // The license sits a step dimmer than the description:
                // same tone, dim modifier — attribution that reads only
                // when looked for.
                if let Some(license) = &skill.license {
                    lines.extend(wrap_line(
                        &Line::from(Span::raw(format!("    {}", license)).fg(theme.text3).dim()),
                        width,
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
                let line = Line::from(vec![
                    marker,
                    Span::raw(subagent.name.clone()).fg(theme.text),
                    Span::raw(format!(" · {}", subagent.source.label())).fg(theme.text3),
                ]);
                lines.push(settings_row(line, selected, theme));
                let model = match &subagent.model {
                    Some(model) => model.clone(),
                    None => "the parent's".to_owned(),
                };
                lines.extend(wrap_line(
                    &Line::from(
                        Span::raw(format!(
                            "    {model} · {} steps",
                            subagent
                                .max_steps
                                .unwrap_or(rhema_subagents::def::DEFAULT_MAX_STEPS),
                        ))
                        .fg(theme.text3),
                    ),
                    width,
                ));
                lines.extend(wrap_line(
                    &Line::from(Span::raw(format!("    {}", subagent.description))).fg(theme.text3),
                    width,
                ));
            }
            (
                lines,
                footer("enter/e - edit in $EDITOR; the built-in is copied to the global dir first"),
                row_hits,
            )
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
                // mutes — the skills page's pair, meanings swapped.
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
            // The rows carry two units: the billed input the
            // providers reported per request — the history rides every
            // request, so it dwarfs the content — and, where the
            // category lives in messages, that content counted once in
            // the log. The note says so where the numbers are read;
            // without it the tool rows read as if the session held
            // tens of millions of tokens of tool output.
            match settings.transcript {
                Some(stats) => {
                    // The divisor doubles as the guard: no requests,
                    // no average, no note.
                    if let Some(avg) = stats.input_tokens.checked_div(stats.requests) {
                        let cache = match stats.cache_hit_rate() {
                            Some(rate) => format!("{:.0}% cached", rate * 100.0),
                            None => "no usage reported yet".to_owned(),
                        };
                        lines.extend(settings_hint(
                            &format!(
                                "billed input, summed over {} requests — the history rides \
                                 every one (~{}/request, {}); the log itself holds {} bytes",
                                stats.requests,
                                usage_k(avg),
                                cache,
                                usage_k(stats.log_bytes as u64),
                            ),
                            theme,
                            width,
                        ));
                    }
                    let total = stats.usage.total();
                    // The of-log figures line up with the breakdown's
                    // row order. The system prompt, the skills, and
                    // the tool definitions ride no message, so their
                    // rows stay billed-only.
                    let of_log = [
                        None,
                        None,
                        Some(stats.log_content.tool_calls),
                        Some(stats.log_content.tool_results),
                        Some(stats.log_content.user_text),
                        Some(stats.log_content.assistant_text),
                    ];
                    for ((label, tokens), once) in stats.usage.labeled().into_iter().zip(of_log) {
                        let pct = if total > 0 {
                            tokens as f64 / total as f64 * 100.0
                        } else {
                            0.0
                        };
                        let value = match once {
                            Some(bytes) => format!(
                                "{:.0}% ({} billed \u{b7} {} of log)",
                                pct,
                                usage_k(tokens),
                                usage_k(bytes as u64),
                            ),
                            None => format!("{:.0}% ({} billed)", pct, usage_k(tokens)),
                        };
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
