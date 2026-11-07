//! The ui module's tests: every renderer runs against a `TestBackend`
//! frame and asserts on the drawn cells.

use super::footer::footer_model;
use super::settings::usage_k;
use super::*;
use crate::palette::DARK;
use crate::screens::{CatalogModel, PickerScreen};
use ratatui::{Terminal, backend::TestBackend};
use rhema_provider::ModelCard;

fn model(id: &str) -> ModelCard {
    ModelCard {
        id: id.to_owned(),
        display_name: id.to_owned(),
        description: String::new(),
        input_token_limit: None,
        output_token_limit: None,
        supports_vision: None,
        generation_methods: vec!["generateContent".to_owned()],
        pricing: None,
    }
}

#[test]
fn the_picker_row_names_the_vision_verdict_only_when_known() {
    let mut card = model("m");
    let row_text = |card: &ModelCard| {
        model_row("google", card, &crate::icons::UNICODE, &DARK)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect::<String>()
    };
    // Unknown says nothing: unknown is not a verdict.
    assert!(!row_text(&card).contains("vision"), "{}", row_text(&card));

    card.supports_vision = Some(true);
    assert!(row_text(&card).contains("◉ vision"), "{}", row_text(&card));

    card.supports_vision = Some(false);
    let row = row_text(&card);
    assert!(row.contains("no vision"), "{row}");
    assert!(
        !row.contains("◉"),
        "the badge is earned, not the note: {row}"
    );
}

/// The Appearance page's toggles: the label column holds a space
/// between the longest label (`Background`, ten cells) and its value.
#[test]
fn the_appearance_labels_keep_a_gap_before_their_values() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Appearance;
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text: String = (0..40)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    // Four toggles render, all values one aligned column over; the
    // longest label (`Background`, ten cells) keeps a space before its
    // value instead of swallowing it.
    // The screen's own placeholder state — nerd icons, native
    // background — is what renders here; the opener overwrites it.
    let row = |label: &str, value: &str| format!("{:<11}{value}", label);
    assert!(text.contains(&row("Background", "Native")), "{text}");
    assert!(text.contains(&row("Colors", "Rhema")), "{text}");
    assert!(text.contains(&row("Theme", "Dark")), "{text}");
    assert!(text.contains(&row("Icons", "Nerd")), "{text}");
    assert!(!text.contains("BackgroundNative"), "{text}");
}

/// The Usage page's detailed split: the subtitle, the six rows as
/// percentages of the total with their kilo-scaled amounts.
#[test]
fn the_usage_page_shows_the_detailed_split() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Usage;
    settings.transcript = Some(rhema_transcript::TranscriptStats {
        requests: 10,
        viewed: 4,
        flushes: 1,
        input_tokens: 90_000,
        cached_input_tokens: 70_000,
        output_tokens: 10_000,
        log_bytes: 4_500,
        log_content: rhema_transcript::LogContent {
            tool_calls: 2_500,
            tool_results: 90_000,
            user_text: 800,
            assistant_text: 1_200,
        },
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
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text: String = (0..40)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(text.contains("Detailed Session Usage"), "{text}");
    assert!(text.contains("summed over 10 requests"), "{text}");
    assert!(text.contains("(~9k/request, 78% cached)"), "{text}");
    // The size lands on its own wrapped row.
    assert!(text.contains("4.5k bytes"), "{text}");
    // Message-backed rows carry the once-counted figure beside the
    // billed one; the system prompt and skills ride no message.
    assert!(text.contains("20% (20k billed)"), "{text}");
    assert!(text.contains("2% (2k billed)"), "{text}");
    assert!(
        text.contains("30% (30k billed \u{b7} 2.5k of log)"),
        "{text}"
    );
    assert!(
        text.contains("30% (30k billed \u{b7} 90k of log)"),
        "{text}"
    );
    assert!(text.contains("8% (8k billed \u{b7} 800 of log)"), "{text}");
    assert!(
        text.contains("10% (10k billed \u{b7} 1.2k of log)"),
        "{text}"
    );
}

/// The amounts scale: kilo past a thousand, mega past a million.
#[test]
fn usage_amounts_scale() {
    assert_eq!(usage_k(0), "0");
    assert_eq!(usage_k(999), "999");
    assert_eq!(usage_k(1_000), "1k");
    assert_eq!(usage_k(42_400), "42.4k");
    assert_eq!(usage_k(1_862_600), "1.9M");
    assert_eq!(usage_k(3_569_100), "3.6M");
}

/// The settings screen draws the whole sidebar with the selected
/// section marked, and the Providers page carries the live instance
/// list with its add row.
#[test]
fn the_settings_screen_draws_the_sidebar_and_the_page() {
    let mut settings = crate::screens::SettingsScreen::new();
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
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
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

/// The Subagents page lists every definition with its provenance and
/// registers one click target per row.
#[test]
fn the_subagents_page_lists_the_definitions_and_their_targets() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Subagents;
    settings.subagents_global_dir = Some("/cfg".into());
    settings.subagents_workspace_dir = "/w/.rhema/subagents".into();
    settings.subagents = vec![
        crate::screens::SubagentRow {
            enabled: true,
            name: "general-purpose".into(),
            description: "Runs one self-contained task in a fresh agent".into(),
            model: None,
            max_steps: None,
            source: crate::screens::SubagentSource::Builtin,
            path: None,
        },
        crate::screens::SubagentRow {
            enabled: false,
            name: "reviewer".into(),
            description: "reviews diffs".into(),
            model: Some("zai/glm".into()),
            max_steps: Some(12),
            source: crate::screens::SubagentSource::Workspace,
            path: Some("/w/.rhema/subagents/reviewer/SUBAGENT.md".into()),
        },
    ];

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("general-purpose · built-in"), "{text}");
    assert!(text.contains("reviewer · workspace"), "{text}");
    assert!(
        !text.contains("the parent"),
        "a definition without a model frontmatter renders no meta line: {text}"
    );
    assert!(
        text.contains("zai/glm · 12 steps max"),
        "overrides render on the meta line: {text}"
    );
    assert!(
        !text.contains("256"),
        "the default step budget is not a setting to re-read per row: {text}"
    );
    assert!(
        text.contains("Runs one self-contained task in a fresh agent"),
        "an off row still lists: {text}"
    );
    assert!(text.contains("●"), "the on dot: {text}");
    assert!(text.contains("○"), "the off dot: {text}");
    assert!(
        text.contains("enter/e - edit in $EDITOR"),
        "the footer hints: {text}"
    );
    assert!(
        text.contains("space - toggle on and off"),
        "the footer hints the toggle: {text}"
    );
    // Every sidebar row plus the two definition rows are clickable.
    assert_eq!(
        hits.len(),
        SettingsSection::ALL.len() + 2,
        "sidebar rows and the two page rows register: {hits:?}"
    );
}

/// A long description renders at most two rows, both indented, the
/// second ending in an ellipsis — never a sprawl from column zero.
#[test]
fn the_subagents_page_caps_the_description_at_two_rows() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Subagents;
    settings.subagents_global_dir = Some("/cfg".into());
    settings.subagents_workspace_dir = "/w/.rhema/subagents".into();
    settings.subagents = vec![crate::screens::SubagentRow {
        enabled: true,
        name: "chatty".into(),
        description: "Takes a long task and breaks it into pieces, \
             reports each one as it lands, and keeps a running summary \
             so the parent can follow along without waiting for the end."
            .into(),
        model: None,
        max_steps: None,
        source: crate::screens::SubagentSource::Workspace,
        path: Some("/w/.rhema/subagents/chatty/SUBAGENT.md".into()),
    }];

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let rows: Vec<String> = (0..24)
        .map(|y| {
            (0..60)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let name_row = rows
        .iter()
        .position(|row| row.contains("chatty · workspace"))
        .expect("the name row renders");
    // The sidebar shares every row, and the border glyphs are
    // multibyte: measure in char columns. The indent puts the prose
    // exactly at the name's own column.
    let name_col = rows[name_row]
        .char_indices()
        .position(|(_, ch)| ch == 'c')
        .expect("the name");
    let indent = |row: &String, col: usize| {
        row.chars()
            .skip(col.saturating_sub(4))
            .take(4)
            .all(char::is_whitespace)
    };
    let first = &rows[name_row + 1];
    let second = &rows[name_row + 2];
    let third = &rows[name_row + 3];
    let takes_col = first
        .char_indices()
        .position(|(_, ch)| ch == 'T')
        .expect("the prose");
    assert!(
        indent(first, name_col),
        "the first row indents: {first:?} / name row: {} (name_col={name_col}, prose_col={takes_col})",
        rows[name_row]
    );
    assert!(
        indent(second, name_col),
        "the second row indents: {second:?}"
    );
    let prose_end = second.trim_end_matches(|ch: char| ch.is_whitespace() || ch == '│');
    assert!(
        prose_end.ends_with('…'),
        "the cap ends in an ellipsis: {second:?}"
    );
    assert!(
        !third.contains("so the parent"),
        "the tail is dropped, no third row: {third:?}"
    );
    assert!(
        first.contains("Takes a long task"),
        "the text starts at the top: {first:?}"
    );
}

/// The Profiles page lists the registry with its details and the
/// add row, one click target each.
#[test]
fn the_profiles_page_lists_the_registry_and_their_targets() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Profiles;
    settings.profiles = vec![
        crate::screens::ProfileRow {
            name: "personal".into(),
            provider: Some("google".into()),
            model: Some("gemini".into()),
            color: Some("#64C9A3".into()),
            usable: true,
            active: true,
        },
        crate::screens::ProfileRow {
            name: "work".into(),
            provider: None,
            model: None,
            color: None,
            usable: false,
            active: false,
        },
    ];

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Profiles"), "the page titles: {text}");
    assert!(
        text.contains("google · gemini · active"),
        "the configured row's details: {text}"
    );
    assert!(
        text.contains("no provider · no API key"),
        "the empty row's details: {text}"
    );
    assert!(text.contains("+ Add a profile..."), "the add row: {text}");
    assert!(
        text.contains("enter switch · c color · d remove"),
        "the footer hints: {text}"
    );
    // Every sidebar row plus the two profile rows and the add row.
    assert_eq!(
        hits.len(),
        SettingsSection::ALL.len() + 3,
        "sidebar and page rows register: {hits:?}"
    );
}

/// The wizard renders its rail and the welcome body; off a
/// graphics terminal (as under TestBackend) the logo rect stays
/// clear and the wordmark stands in.
#[test]
fn the_wizard_renders_its_rail_and_welcome() {
    let wizard = crate::onboarding::OnboardingScreen::new(
        vec![crate::screens::SkillRow {
            name: "unslop".into(),
            description: "Remove AI patterns.".into(),
            license: Some("MIT License, Copyright (c) 2026 Lauren Tan".into()),
            source: crate::screens::SkillSource::Bundled,
            enabled: true,
        }],
        "personal".into(),
        false,
        rhema_config::Profile::default(),
    );
    let mut logo_rect = None;
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| {
            render_onboarding(
                frame,
                &wizard,
                &crate::icons::UNICODE,
                &DARK,
                &mut logo_rect,
                &mut Vec::new(),
            )
        })
        .unwrap();
    let text: String = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Rhema Code"), "the wordmark: {text}");
    assert!(text.contains("Welcome to rhema"), "the panel title: {text}");
    for step in crate::onboarding::OnboardingStep::ALL {
        assert!(
            text.contains(step.label()),
            "{} in the rail: {text}",
            step.label()
        );
    }
    assert!(text.contains("revisitable later in /settings"));
    assert!(
        !logo_rect.is_some(),
        "no graphics protocol under TestBackend: {logo_rect:?}"
    );
    // The skills step carries the license line, dimmer than the
    // description (same tone, dim modifier).
    let mut wizard = crate::onboarding::OnboardingScreen::new(
        vec![crate::screens::SkillRow {
            name: "unslop".into(),
            description: "Remove AI patterns.".into(),
            license: Some("MIT License, Copyright (c) 2026 Lauren Tan".into()),
            source: crate::screens::SkillSource::Bundled,
            enabled: true,
        }],
        "personal".into(),
        false,
        rhema_config::Profile::default(),
    );
    wizard.go(crate::onboarding::OnboardingStep::Skills);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| {
            render_onboarding(
                frame,
                &wizard,
                &crate::icons::UNICODE,
                &DARK,
                &mut logo_rect,
                &mut Vec::new(),
            )
        })
        .unwrap();
    let text: String = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Lauren Tan"),
        "the license shows on the skills step: {text}"
    );
    let dim = (0..30).any(|y| {
        (0..100).any(|x| {
            terminal.backend().buffer()[(x, y)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        })
    });
    assert!(dim, "the license renders dimmed: {text}");
}

/// A page's rows render inside the pane the sidebar leaves: the
/// title, the rule, and the wrapped content.
#[test]
fn the_page_renders_its_title_and_rule() {
    let settings = crate::screens::SettingsScreen::new();

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
        text.contains("Providers"),
        "the opening page carries its title: {text}"
    );
    assert!(text.contains('─'), "the title sits over a rule: {text}");

    let mut settings = settings;
    settings.section = SettingsSection::About;
    let text = draw(&settings);
    assert!(
        text.contains("About"),
        "the section switch repaints the title: {text}"
    );
}

/// The add dialog pops over the settings: its provider list draws,
/// and its rows register as click targets while it shows.
#[test]
fn the_add_dialog_draws_its_provider_list() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.dialog = Some(crate::screens::AddInstanceDialog::new(&settings.stored));

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
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
    );
    assert_eq!(picker.selected, 0);

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render_picker(frame, &picker, &crate::icons::UNICODE, &DARK))
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
    );
    picker.failed = vec!["openrouter"];

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render_picker(frame, &picker, &crate::icons::UNICODE, &DARK))
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
    let mut picker = PickerScreen::new(models);
    picker.selected = 19;

    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| render_picker(frame, &picker, &crate::icons::UNICODE, &DARK))
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

/// Before the flow's first update lands, the dialog names the wait:
/// a spinner over "loading…", not an empty box.
#[test]
fn the_flow_stage_shows_a_spinner_before_its_first_update() {
    let mut dialog = crate::screens::AddInstanceDialog::new(&rhema_config::Profile::default());
    dialog.stage = crate::screens::DialogStage::Flow;
    dialog.flow_started = Some(std::time::Instant::now());
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::dialogs::render_add_dialog(
                frame,
                &dialog,
                &crate::palette::DARK,
                &mut Vec::new(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
        .collect();
    // Frame zero of the braille spinner, the note beside it.
    assert!(screen.contains('⠋'), "{screen}");
    assert!(screen.contains("loading…"), "{screen}");
}

/// Once the flow reports, the loading line steps aside: the
/// instruction and the progress note are the only body.
#[test]
fn the_loading_line_leaves_once_the_flow_reports() {
    let mut dialog = crate::screens::AddInstanceDialog::new(&rhema_config::Profile::default());
    dialog.stage = crate::screens::DialogStage::Flow;
    dialog.flow_started = Some(std::time::Instant::now());
    dialog.flow_waiting = Some("waiting for you to finish in the browser".into());
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::dialogs::render_add_dialog(
                frame,
                &dialog,
                &crate::palette::DARK,
                &mut Vec::new(),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
        .collect();
    assert!(!screen.contains("loading…"), "{screen}");
    assert!(screen.contains("waiting for you to finish"), "{screen}");
}

#[cfg(all(test, feature = "mock"))]
mod scroll_tests {
    use crate::app::{App, AppSetup, RunOptions};
    use crate::events::OwnedEvent;
    use crate::palette::DARK;
    use crate::palette::ThemeKind;
    use crate::ui::footer::footer_folder;
    use crate::ui::input::{
        cursor_at_rows, format_bytes, image_chip_label, input_layout, input_rows,
    };
    use crate::ui::status::elapsed_text;
    use crate::ui::{render, wrap_line};
    use ratatui::text::Line;
    use std::collections::BTreeMap;
    use unicode_width::UnicodeWidthStr;

    use crate::screens::{CatalogModel, ChatState, Chip};
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
        let config = crate::store::Config {
            settings: rhema_config::Settings {
                profiles: BTreeMap::from([("personal".to_owned(), personal)]),
                ..rhema_config::Settings::default()
            },
            ..Default::default()
        };
        App::new(AppSetup {
            store: Arc::new(MemoryStore::default()),
            checkpoint: None,
            config,
            backend: Some(crate::app::Backend::Google {
                api_key: "k".into(),
            }),
            session: rhema_core::Session::default(),
            secrets: None,
            sandboxed: None,
            sandbox_machines: crate::sandbox::SandboxMachines::default(),
            home: Some(std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id()))),
            onboard: false,
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: ThemeKind::Dark,
                background: crate::palette::Background::Rhema,
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

    /// A background subagent job pins its live line under the spinner,
    /// read off the wire's registry at every redraw: working, it spins
    /// with the delegate's name and job id and phase; finished, the
    /// line leaves at once — the transcript's finish notice carries the
    /// outcome now, and the strip pins working jobs only.
    #[tokio::test]
    async fn a_working_subagent_pins_its_line_under_the_spinner() {
        use rhema_core::{EventSink, ProviderError};
        use rhema_subagents::{SubagentDef, SubagentTools};

        /// Never answers: a job parked in its model stream.
        struct Hanging;
        /// Answers one report and ends.
        struct Report;

        #[async_trait::async_trait]
        impl rhema_core::ModelProvider for Hanging {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                std::future::pending().await
            }
        }

        #[async_trait::async_trait]
        impl rhema_core::ModelProvider for Report {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                Ok(rhema_core::ModelOutput {
                    message: rhema_core::Message::text(rhema_core::Role::Assistant, "the report"),
                    stop_reason: rhema_core::StopReason::EndTurn,
                    usage: rhema_core::Usage::default(),
                })
            }
        }

        let mut app = chat_app();
        app.turn.running = true;
        let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "general-purpose".into(),
            description: "explore".into(),
            model: None,
            max_steps: None,
            body: "Explore.".into(),
        };
        let tools = Arc::new(SubagentTools::new(Arc::new(Hanging), vec![def], shell, "m"));
        app.subagents = Some(tools.handle());
        let start = rhema_core::ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "general-purpose", "prompt": "explore"}),
            ..rhema_core::ToolCall::default()
        };
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            start.clone(),
            &rhema_core::ToolContext::default(),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app
            .subagent_jobs()
            .first()
            .is_none_or(|job| job.phase == "queued")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
        assert!(screen.contains("general-purpose (job-1)"), "{screen}");
        assert!(screen.contains("thinking"), "{screen}");

        // Finished: the line leaves at once; the finish event names the
        // outcome the transcript notice will carry.
        let tools = {
            let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
            let def = SubagentDef {
                name: "general-purpose".into(),
                description: "explore".into(),
                model: None,
                max_steps: None,
                body: "Explore.".into(),
            };
            Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
        };
        app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            start,
            &rhema_core::ToolContext::default(),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let finished = loop {
            let events = app.subagents.as_ref().unwrap().take_finished();
            if !events.is_empty() {
                break events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the job never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(
            finished[0].line,
            "Subagent general-purpose (job-1) has finished."
        );
        let screen = screen_text(&mut app);
        assert!(
            !screen.contains("general-purpose (job-1)"),
            "a finished job is not pinned: {screen}"
        );

        // The fetch still serves the cached report; the strip stays
        // empty either way.
        let fetch = rhema_core::ToolCall {
            id: "c2".into(),
            name: "subagent".into(),
            args: serde_json::json!({"id": "job-1"}),
            ..rhema_core::ToolCall::default()
        };
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            fetch,
            &rhema_core::ToolContext::default(),
        )
        .await;
        let screen = screen_text(&mut app);
        assert!(!screen.contains("general-purpose (job-1)"), "{screen}");
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
        let mut settings = crate::screens::SettingsScreen::new();
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
                crate::ui::settings::render_settings(
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
                crate::ui::dialogs::render_add_dialog(
                    frame,
                    &dialog,
                    &crate::palette::DARK,
                    &mut Vec::new(),
                )
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

    /// The window title: the mark, then the workspace as the footer
    /// spells it — the home trim, the capped parents. No badge glyph:
    /// title bars render plain text, and the badge arrives as tofu.
    #[test]
    fn the_window_title_reads_like_the_footer() {
        use crate::ui::footer::window_title;
        let mut app = chat_app();
        app.home = Some(PathBuf::from("/home/u"));
        app.options.workspace = PathBuf::from("/home/u/Documents/prjs/rhema");
        assert_eq!(window_title(&app), "ῥ · prjs/rhema");
        // Deeper than the footer's three folders: the ellipsis rides.
        app.options.workspace = PathBuf::from("/home/u/prjs/a/b/c/d/e");
        assert_eq!(window_title(&app), "ῥ · …/c/d/e");
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

    /// The footer always names where bash runs: local reads as a fact,
    /// and a running sandbox shows its kind — or its instance's name
    /// when the profile's active pointer names it.
    #[test]
    fn the_footer_names_where_bash_runs() {
        let mut app = chat_app();
        app.config.profiles.get_mut("personal").unwrap().provider = Some("google".to_owned());
        assert!(
            screen_text(&mut app).contains("▣ local"),
            "the local fast path is a fact: {}",
            screen_text(&mut app)
        );
        app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: crate::args::SandboxChoice::Docker,
            target: "rhema-dev".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        });
        app.config.set_for_profile(Some("personal"), |profile| {
            profile.sandbox = Some(rhema_config::SandboxSettings {
                kind: Some("docker".to_owned()),
                active: Some("dev".to_owned()),
                ..rhema_config::SandboxSettings::default()
            });
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
        });
        assert!(
            screen_text(&mut app).contains("▣ dev"),
            "the instance's name is where bash runs: {}",
            screen_text(&mut app)
        );
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

        // Wide enough for every segment at once: the no-provider
        // footer alone already filled eighty columns, and the sandbox
        // badge made the tail clip there. What this test pins is which
        // badge shows, not a width budget.
        let draw = |app: &mut App| {
            let backend = TestBackend::new(100, 12);
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
        // Wide enough for every segment at once — the badge and the
        // gauge are the subject, not a width budget (see the allowance
        // test's note).
        let draw = |app: &mut App| {
            let backend = TestBackend::new(100, 12);
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
                supports_vision: None,
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
        app.transcript.push_user(&long, Vec::new());

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
        app.transcript.push_user(
            &rhema_core::paste::embed(
                &format!("look at {} this", ChatState::MARK),
                &[long.to_owned()],
            ),
            Vec::new(),
        );
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

    #[test]
    fn a_submitted_image_renders_as_a_chip_after_the_prompt() {
        let mut app = chat_app();
        let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
        app.transcript.push_user("what is this?", vec![image]);
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        // The chip names format and size; the payload itself never
        // shows.
        assert!(screen.contains("what is this? ▣1 PNG 6 B"), "{screen}");
        assert!(!screen.contains("aGVsbG8="), "{screen}");
    }

    #[test]
    fn an_image_chip_is_one_atomic_cell_like_a_paste_chip() {
        let input = format!("before{} after", ChatState::MARK);
        let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
        let rows = input_layout(&input, &[Chip::Image(image)], 40, &DARK);
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
        assert_eq!(rendered, "❯ before ▣1 PNG 6 B  after");
        // The cursor crosses the twelve-cell chip in one step: on its
        // first cell, then past it on the following character.
        assert_eq!(cursor_at_rows(&rows, 6), Some((0, 6)));
        assert_eq!(cursor_at_rows(&rows, 7), Some((0, 18)));
    }

    #[test]
    fn image_chips_label_format_and_decoded_size() {
        assert_eq!(format_bytes(6), "6 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(12_600), "12.3 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");

        for (media_type, tag) in [
            (rhema_core::ImageMediaType::Png, "PNG"),
            (rhema_core::ImageMediaType::Jpeg, "JPEG"),
            (rhema_core::ImageMediaType::Gif, "GIF"),
            (rhema_core::ImageMediaType::Webp, "WEBP"),
        ] {
            let image = rhema_core::Image::new(media_type, "aGVsbG8=");
            assert_eq!(image_chip_label(&image, 0), format!("▣1 {tag} 6 B"));
        }
        // The size reads the decoded bytes, not the base64 length: a
        // 16800-character payload is 12600 bytes.
        let image = rhema_core::Image::new(rhema_core::ImageMediaType::Jpeg, "a".repeat(16_800));
        assert_eq!(image_chip_label(&image, 2), "▣3 JPEG 12.3 KB");
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
    fn the_retry_wait_counts_down_in_the_status_line() {
        let mut app = chat_app();
        app.turn.running = true;
        // Wide enough that the wait's full line fits: error, attempt,
        // countdown, and the cancel hint.
        let draw = |app: &mut App| {
            let backend = TestBackend::new(100, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect::<String>()
        };

        // The provider died mid-turn: the strip names the error, the
        // attempt the ladder is on, and the wait it announced.
        app.on_event(crate::events::UiEvent::Engine(
            crate::events::OwnedEvent::RetryWait {
                error: "network error: dead link".into(),
                delay: std::time::Duration::from_secs(30),
                attempt: 1,
                total: 6,
            },
        ));
        let screen = draw(&mut app);
        assert!(screen.contains("network error: dead link"), "{screen}");
        assert!(screen.contains("attempt 1 of 6"), "{screen}");
        assert!(screen.contains("retrying in"), "{screen}");
        assert!(screen.contains("esc cancels"), "{screen}");

        // The retry connects and streams: the wait is gone, the phase
        // is back to thinking.
        app.on_event(crate::events::UiEvent::Engine(
            crate::events::OwnedEvent::TextDelta("back".into()),
        ));
        let screen = draw(&mut app);
        assert!(screen.contains("thinking"), "{screen}");
        assert!(!screen.contains("retrying in"), "{screen}");
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
        app.transcript.push_user("look at this", Vec::new());

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
                    supports_vision: None,
                    generation_methods: vec!["generateContent".into()],
                    pricing: None,
                }]),
            }],
        });
        assert!(!app.models.is_empty(), "the catalog is cached");
        for c in "/model ".chars() {
            app.chat.handle_key(
                key(KeyCode::Char(c)),
                &app.models,
                &[],
                &[],
                &crate::icons::UNICODE,
            );
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
            app.chat.handle_key(
                key(KeyCode::Char(c)),
                &app.models,
                &[],
                &[],
                &crate::icons::UNICODE,
            );
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
        // Sixteen commands, six visible rows: nine downs put the
        // selection on /retry (the row /resume held before it joined
        // the table), and the window must have followed it.
        for _ in 0..9 {
            app.chat
                .handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
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
            screen.contains("/retry"),
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
        let paste = Chip::Paste("x\ny\nz\nw".to_owned());
        assert_eq!(input_rows(&marked, &[paste.clone(), paste], 40, &DARK), 3);
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
        let layout = input_layout(&marked, &[Chip::Paste("x\ny".to_owned())], 40, &DARK);
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
            &[
                Chip::Paste("l1\nl2\nl3\nl4".to_owned()),
                Chip::Paste("m1\nm2\nm3\nm4\nm5".to_owned()),
            ],
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
        let layout = input_layout(&marked, &[Chip::Paste("x\ny".to_owned())], 12, &DARK);
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
        let rows = input_layout(
            &input,
            &[Chip::Paste("l1\nl2\nl3\nl4\nl5".to_owned())],
            40,
            &DARK,
        );
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
            .push_user(&format!("{} end", "word ".repeat(200)), Vec::new());

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
        app.transcript.push_user("investigate", Vec::new());
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
        app.transcript.push_user("alpha\nbeta\ngamma", Vec::new());

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
        app.transcript.push_user("what skills", Vec::new());
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
        app.transcript.push_user("investigate", Vec::new());
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
        // Flush right: one cell follows the code — the pane's right
        // text padding. The old char-count math left the code a cell
        // short for every newline it silently dropped.
        let code_ends = row.rfind("[0]").unwrap() + "[0]".len();
        let trailing = buffer.area.width as usize - code_ends;
        assert_eq!(trailing, 1, "code flush at the text edge: {row}");
    }

    #[test]
    fn a_multiline_command_still_right_aligns_the_code() {
        let command = format!("python3 - <<'EOF'\n{}EOF", "print(1)\n".repeat(20));
        // The wire carries the newlines escaped, as JSON requires.
        let args = format!(r#"{{"command": "{}"}}"#, command.replace('\n', "\\n"));
        let line = crate::ui::pane::tool_row(&args, Some(true), Some(0), 60, &DARK);
        let drawn: usize = line
            .spans
            .iter()
            .map(|span| span.content.to_string().width())
            .sum();
        assert_eq!(
            drawn, 60,
            "the code sits at the edge, not left of it: {line:?}"
        );
    }

    #[test]
    fn empty_assistant_noise_between_tools_stays_tight() {
        let mut app = chat_app();
        app.transcript.push_user("investigate", Vec::new());
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
        app.transcript.push_user("investigate", Vec::new());
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
        app.transcript.push_user("alpha", Vec::new());
        app.transcript
            .apply(&crate::events::OwnedEvent::TextDelta("beta reply".into()));
        app.transcript.push_user("gamma", Vec::new());

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

/// The MCP page lists the servers with their transport, the muted
/// mark, the skip notice, and the empty state; every server row and
/// the add row register click targets.
#[test]
fn the_mcp_page_lists_the_servers_and_their_marks() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Mcp;
    settings.mcp = vec![
        crate::screens::McpServerRow {
            name: "atlassian".into(),
            transport: Some("stdio"),
            description: "Jira and Confluence".into(),
            disabled: true,
            notice: None,
        },
        crate::screens::McpServerRow {
            name: "broken".into(),
            transport: None,
            description: "both transports named".into(),
            disabled: false,
            notice: Some(
                "mcp server `broken` skipped: `command` and `url` are exclusive; pick one".into(),
            ),
        },
    ];

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text = (0..24)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("atlassian · stdio · muted"), "{text}");
    assert!(text.contains("broken · unresolvable"), "{text}");
    assert!(text.contains("Jira and Confluence"), "{text}");
    assert!(
        text.contains("mcp server `broken` skipped"),
        "the notice shows under its row: {text}"
    );
    assert!(text.contains("+ Add a server..."), "the add row: {text}");
    assert!(
        text.contains("enter mutes · d removes"),
        "the footer hints: {text}"
    );
    // Every sidebar row plus the two server rows and the add row are
    // clickable.
    assert_eq!(
        hits.len(),
        SettingsSection::ALL.len() + 3,
        "sidebar, page, and add rows register: {hits:?}"
    );

    // An empty profile names the fact and the way out.
    settings.mcp.clear();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text = (0..24)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("no servers in this profile"), "{text}");
    assert!(
        text.contains("the add row below installs one"),
        "the empty state nudges at the add row: {text}"
    );
}
