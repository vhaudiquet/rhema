//! The first-run wizard: the logo rail, the connected step, and the body
//! each action renders.

use super::*;

/// The logo's height in cell rows; the width doubles it because cells
/// are roughly twice as tall as wide — the image stays square.
const ONBOARDING_LOGO_ROWS: u16 = 10;

/// The wizard's rail width: wide enough for the logo and the step
/// labels, narrow enough to leave the body its room.
const ONBOARDING_RAIL: u16 = 26;

/// The onboarding wizard: the logo rail on the left (the terminal
/// graphics image on kitty and Ghostty, the wordmark elsewhere), the
/// step's body on the right, one footer of hints. The same chrome the
/// settings screen wears.
pub(crate) fn render_onboarding(
    frame: &mut Frame,
    wizard: &crate::onboarding::OnboardingScreen,
    icons: &crate::icons::Icons,
    theme: &Theme,
    logo_rect: &mut Option<Rect>,
    hits: &mut Vec<(Rect, crate::app::OnboardingHit)>,
) {
    let area = frame.area();
    let block = crate::chrome::panel(Some(" welcome to rhema "), theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [rail_area, page_area] =
        Layout::horizontal([Constraint::Length(ONBOARDING_RAIL), Constraint::Min(24)]).areas(inner);
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(page_area);
    let text_width = content_area.width.saturating_sub(4) as usize;

    // The rail: the logo, the wordmark, a rule, then the steps — the
    // current one marked, the rest dim.
    let mut rail = Vec::new();
    if crate::term_image::supported() {
        // The image composites over these reserved rows after the
        // frame; the rect is what the runner places against.
        *logo_rect = Some(Rect {
            x: rail_area.x + 1,
            y: rail_area.y + 1,
            width: ONBOARDING_LOGO_ROWS * 2,
            height: ONBOARDING_LOGO_ROWS,
        });
        for _ in 0..ONBOARDING_LOGO_ROWS {
            rail.push(Line::default());
        }
    } else {
        // The wordmark fallback: the banner's mark, larger in spirit if
        // not in cells.
        rail.push(Line::from(vec![
            Span::raw("ῥ").fg(theme.accent_text).bold(),
        ]));
        rail.push(Line::default());
    }
    rail.push(Line::from(Span::raw("Rhema Code").bold()));
    rail.push(Line::from(Span::raw(crate::VERSION_LABEL).fg(theme.text3)));
    rail.push(Line::default());
    rail.push(Line::from(crate::chrome::rule(
        rail_area.width.saturating_sub(2) as usize,
        theme,
    )));
    for step in crate::onboarding::OnboardingStep::ALL {
        // The rail line this row will render at — the hit rect's y
        // counts the top padding in, the way the settings sidebar's
        // rects do.
        let rail_line = rail.len();
        let current = *step == wizard.step;
        let style = if current {
            Style::default().fg(theme.accent_text).bold()
        } else {
            Style::default().fg(theme.text2)
        };
        rail.push(Line::from(vec![
            crate::chrome::marker(current, theme),
            Span::styled(step.label().to_owned(), style),
        ]));
        hits.push((
            Rect {
                x: rail_area.x + 1,
                y: rail_area.y + 1 + rail_line as u16,
                width: rail_area.width.saturating_sub(2),
                height: 1,
            },
            crate::app::OnboardingHit::RailStep(*step),
        ));
    }
    frame.render_widget(
        Paragraph::new(rail).block(Block::default().padding(Padding::new(1, 1, 1, 0))),
        rail_area,
    );

    let (body, footer) = onboarding_body(wizard, icons, theme, text_width);
    frame.render_widget(
        Paragraph::new(body).block(Block::default().padding(Padding::new(2, 2, 1, 0))),
        content_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(footer)).block(Block::default().padding(Padding::horizontal(2))),
        footer_area,
    );
}

/// The "currently configured" block the wizard shows on the Welcome
/// and Login steps when a config already exists — a `rhema onboard`
/// rerun must show what is connected, not start from a blank slate.
pub(crate) fn connected_lines(
    stored: &rhema_config::Profile,
    profile_name: &str,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let provider = stored.provider_name().map(|name| match &stored.instance {
        Some(instance) => format!("{name} ({instance})"),
        None => name.to_owned(),
    });
    if provider.is_none() && stored.model.is_none() {
        return lines;
    }
    lines.push(Line::from(Span::raw("currently configured").bold()));
    lines.push(Line::from(crate::chrome::rule(width, theme)));
    let value = |text: String| Span::raw(text).fg(theme.text);
    if let Some(provider) = provider {
        lines.push(Line::from(vec![
            Span::raw("provider  ").fg(theme.text3),
            value(provider),
        ]));
    }
    if let Some(model) = &stored.model {
        lines.push(Line::from(vec![
            Span::raw("model     ").fg(theme.text3),
            value(model.clone()),
        ]));
    }
    lines.push(Line::from(vec![
        Span::raw("profile   ").fg(theme.text3),
        value(profile_name.to_owned()),
    ]));
    lines.push(Line::default());
    lines
}

/// The step's body lines and the footer hints. The dialog, while the
/// login step runs one, renders inline — the same stages the settings'
/// modal shows, in the wizard's own pane.
pub(crate) fn onboarding_body(
    wizard: &crate::onboarding::OnboardingScreen,
    icons: &crate::icons::Icons,
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Span<'static>>) {
    use crate::onboarding::OnboardingStep as Step;
    let footer = |hints: &str| vec![Span::raw(hints.to_owned()).fg(theme.text3)];
    let title = |text: &str, lines: &mut Vec<Line<'static>>| {
        lines.push(Line::from(Span::raw(text.to_owned()).bold()));
        lines.push(Line::from(crate::chrome::rule(width, theme)));
        lines.push(Line::default());
    };
    let mut lines = Vec::new();
    let hints = match wizard.step {
        Step::Welcome => {
            title("Rhema Code", &mut lines);
            lines.extend(connected_lines(
                &wizard.stored,
                &wizard.profile_start,
                theme,
                width,
            ));
            lines.extend(wrap_line(
                &Line::from(
                    Span::raw(
                        "A coding agent for the terminal: it reads and edits the tree, \
                         runs the commands, and keeps the conversation.",
                    )
                    .fg(theme.text),
                ),
                width,
            ));
            for text in [
                "A few choices set it up — a provider login, a model, a profile,",
                "the colors, and the skills rhema ships with. Everything is",
                "revisitable later in /settings.",
            ] {
                lines.push(Line::from(Span::raw(text).fg(theme.text2)));
            }
            footer("enter start · esc quit")
        }
        Step::Login => {
            if let Some(dialog) = &wizard.dialog {
                title("Add a login", &mut lines);
                match dialog.stage {
                    crate::screens::DialogStage::SelectProvider => {
                        for (index, entry) in PROVIDERS.iter().enumerate() {
                            let selected = index == dialog.selected;
                            let style = if selected {
                                Style::default().fg(theme.text).bold()
                            } else {
                                Style::default().fg(theme.text2)
                            };
                            lines.push(Line::from(vec![
                                crate::chrome::marker(selected, theme),
                                Span::styled(entry.name.to_owned(), style),
                            ]));
                        }
                    }
                    crate::screens::DialogStage::NameInstance
                    | crate::screens::DialogStage::EnterCredential => {
                        lines.push(Line::from(vec![
                            Span::raw(dialog.input_label()).fg(theme.text3),
                            Span::raw(dialog.masked_input()).bold(),
                        ]));
                    }
                    crate::screens::DialogStage::Flow => {
                        lines.extend(dialog_flow_lines(dialog, theme, width));
                    }
                }
                let status = match &dialog.status {
                    crate::screens::DialogStatus::Idle => {
                        Line::from(dialog.input_hint()).fg(theme.text3)
                    }
                    crate::screens::DialogStatus::Validating => {
                        Line::from("validating…").fg(theme.text3)
                    }
                    crate::screens::DialogStatus::Error(message) => {
                        Line::from(message.clone()).fg(theme.error)
                    }
                };
                lines.push(Line::default());
                lines.push(status);
                footer("esc back")
            } else {
                title("A provider to serve the turns", &mut lines);
                // The logins this profile already holds — a rerun
                // shows what is connected before offering another.
                // Owned copies: the rows must not borrow the snapshot
                // the loop reads.
                let entries: Vec<(String, String)> = wizard
                    .stored
                    .instance_entries()
                    .into_iter()
                    .map(|(name, instance)| (name.to_owned(), instance.provider.clone()))
                    .collect();
                let active = wizard
                    .stored
                    .active_instance()
                    .map(|(name, _)| name.to_owned());
                for (name, provider) in entries {
                    let dot = if Some(name.as_str()) == active.as_deref() {
                        theme.ok
                    } else {
                        theme.text3
                    };
                    lines.push(Line::from(vec![
                        Span::raw("● ").fg(dot),
                        Span::raw(name).fg(theme.text2),
                        Span::raw(format!(" ({provider})")).fg(theme.text3),
                    ]));
                }
                if !wizard.stored.instances.is_empty() {
                    lines.push(Line::default());
                }
                let rows = ["+ add a login", "skip for now"];
                for (index, row) in rows.iter().enumerate() {
                    let selected = index == wizard.row;
                    lines.push(Line::from(vec![
                        crate::chrome::marker(selected, theme),
                        Span::raw((*row).to_owned()).fg(if selected {
                            theme.text
                        } else {
                            theme.text2
                        }),
                    ]));
                }
                lines.push(Line::default());
                let note = if wizard.backend_ready || !wizard.stored.instances.is_empty() {
                    "a provider is already configured — adding another is optional"
                } else {
                    "the login validates by fetching the provider's model list"
                };
                lines.push(Line::from(Span::raw(note).fg(theme.text3)));
                footer("enter select · esc back")
            }
        }
        Step::Model => {
            title("Pick a model", &mut lines);
            let mut index = 0;
            if let Some((_, _, id)) = wizard.keep_current_model() {
                let selected = index == wizard.row;
                lines.push(Line::from(vec![
                    crate::chrome::marker(selected, theme),
                    Span::raw(id.clone()).fg(if selected { theme.text } else { theme.text2 }),
                    Span::raw("  · keep current").fg(theme.text3),
                ]));
                index += 1;
            }
            if wizard.models.is_empty() && index == 0 {
                lines.push(Line::from(Span::raw("no models").fg(theme.text3)));
            }
            for entry in &wizard.models {
                let selected = index == wizard.row;
                let mut line = Line::from(vec![crate::chrome::marker(selected, theme)]);
                line.spans
                    .extend(model_row(entry.provider, &entry.model, icons, theme).spans);
                if selected {
                    line = line.style(Style::default().bg(theme.elevated));
                }
                lines.push(line);
                index += 1;
            }
            footer("enter choose · esc back")
        }
        Step::Profile => {
            title("The default profile", &mut lines);
            lines.push(Line::from(vec![
                Span::raw("name: ").fg(theme.text3),
                Span::raw(wizard.profile_name.clone()).bold(),
                Span::raw("▌").fg(theme.accent_text),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(
                Span::raw("profiles keep settings apart — work and personal side by side")
                    .fg(theme.text3),
            ));
            footer("type to edit · enter continue · esc back")
        }
        Step::Appearance => {
            title("Colors", &mut lines);
            for (index, palette) in crate::palette::Palette::ALL.iter().enumerate() {
                let selected = index == wizard.row;
                lines.push(Line::from(vec![
                    crate::chrome::marker(selected, theme),
                    Span::raw(palette.label().to_owned()).fg(if selected {
                        theme.text
                    } else {
                        theme.text2
                    }),
                ]));
            }
            lines.push(Line::default());
            lines.push(Line::from(
                Span::raw("moving previews the palette live; enter keeps it").fg(theme.text3),
            ));
            footer("↑↓ preview · enter continue · esc back")
        }
        Step::Skills => {
            title("Bundled skills", &mut lines);
            if wizard.skills.is_empty() {
                lines.push(Line::from(
                    Span::raw("no skills discovered").fg(theme.text3),
                ));
            }
            for (index, skill) in wizard.skills.iter().enumerate() {
                let selected = index == wizard.row;
                let dot = if skill.enabled { "●" } else { "○" };
                let dot_color = if skill.enabled { theme.ok } else { theme.text3 };
                let line = Line::from(vec![
                    crate::chrome::marker(selected, theme),
                    Span::raw(dot).fg(dot_color),
                    Span::raw(" ").fg(theme.text3),
                    Span::raw(skill.name.clone()).fg(if skill.enabled {
                        theme.text
                    } else {
                        theme.text3
                    }),
                    Span::raw(format!(" · {}", skill.source.label())).fg(theme.text3),
                ]);
                lines.push(if selected {
                    line.style(Style::default().bg(theme.elevated))
                } else {
                    line
                });
                lines.push(Line::from(
                    Span::raw(format!("    {}", skill.description)).fg(theme.text3),
                ));
                // The license, a step dimmer than the description — the
                // same treatment the settings Skills page gives it.
                if let Some(license) = &skill.license {
                    lines.push(Line::from(
                        Span::raw(format!("    {}", license)).fg(theme.text3).dim(),
                    ));
                }
            }
            footer("space/t toggle · enter continue · esc back")
        }
        Step::Finish => {
            title("Ready", &mut lines);
            let provider = wizard
                .chosen_provider
                .clone()
                .unwrap_or_else(|| "none yet — /login later".to_owned());
            let model = wizard
                .chosen_model
                .clone()
                .unwrap_or_else(|| "none yet — /model later".to_owned());
            lines.push(Line::from(vec![
                Span::raw("provider  ").fg(theme.text3),
                Span::raw(provider).fg(theme.text),
            ]));
            lines.push(Line::from(vec![
                Span::raw("model     ").fg(theme.text3),
                Span::raw(model).fg(theme.text),
            ]));
            lines.push(Line::from(vec![
                Span::raw("profile   ").fg(theme.text3),
                Span::raw(wizard.profile_start.clone()).fg(theme.text),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(
                Span::raw("Everything lives in /settings; rhema onboard runs this wizard again.")
                    .fg(theme.text2),
            ));
            footer("enter finish")
        }
    };
    if let Some(error) = &wizard.error {
        lines.push(Line::default());
        lines.extend(wrap_line(
            &Line::from(Span::raw(error.clone())).fg(theme.error),
            width,
        ));
    }
    let hints: Vec<Span<'static>> = hints;
    let lines: Vec<Line<'static>> = lines;
    (lines, hints)
}
