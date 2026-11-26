//! The ui module's tests: every renderer runs against a `TestBackend`
//! frame and asserts on the drawn cells.

use super::footer::footer_model;
use super::settings::usage_k;
use super::*;
use crate::palette::DARK;
use crate::screens::{CatalogModel, CurrentModel, ModelPicker, PickerScreen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{Terminal, backend::TestBackend};
use rhema_provider::ModelCard;
use std::collections::{BTreeMap, BTreeSet};

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
        family: None,
        release_date: None,
    }
}

/// The bubble is the drawn frame on every terminal, and it carries no
/// background: the cells stay on the page's ground, so nothing bleeds
/// around it, `background: native` keeps the terminal's own ground
/// inside it, and the protocol card, when the terminal takes one,
/// backs the frame instead of replacing it. The geometry rides along
/// for the runner's card.
#[test]
fn the_bubble_is_a_backgroundless_frame_with_its_geometry() {
    let (rows, geometry) = super::pane::render_user("hi", &[], &[], &DARK, 40);
    let geometry = geometry.expect("the bubble reports its geometry");
    assert_eq!(rows.len(), 3, "cap, body, foot: {rows:?}");
    assert_eq!(geometry.rows, 3);
    // "hi" plus one pad cell a side and the two `│` glyphs.
    assert_eq!(geometry.width, 6);
    assert_eq!(geometry.left, 34);
    let text: String = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains('╭'), "the drawn frame: {text}");
    assert!(text.contains('│'), "the drawn frame: {text}");
    assert!(text.contains("hi"), "{text}");
    // No cell carries a background: the frame is ink on the page.
    for row in &rows {
        for span in &row.spans {
            assert_eq!(
                span.style.bg,
                None,
                "no background on {:?}",
                span.content.as_ref()
            );
        }
    }
}

/// The panel's frame reads on either ground, and the title never
/// inherits the border's dim ink: on a native ground the frame lifts to
/// the readable token because there is no panel fill behind it.
#[test]
fn the_panel_frame_and_title_read_on_both_grounds() {
    let inks = |theme: &crate::palette::Theme| {
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(super::chrome::panel(Some("Welcome"), theme), frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let x = (0..40)
            .find(|&x| buffer[(x, 0)].symbol() == "W")
            .expect("the title starts on the top border");
        (
            buffer[(x, 0)].style().fg.expect("the title has ink"),
            buffer[(0, 1)].style().fg.expect("the frame has ink"),
        )
    };
    assert_eq!(inks(&DARK), (DARK.text, DARK.border_strong));
    let native = DARK.painted(crate::palette::Background::Native);
    assert_eq!(inks(&native), (DARK.text, DARK.text3));
}

/// The add dialog carries its own frame and title ink, so the title
/// never takes the dim border fg and the box reads on either ground.
/// The backdrop loses brightness but keeps its glyphs: a terminal has
/// no pixels to blur, so the modal haze is a contrast fade.
#[test]
fn the_modal_backdrop_fades_the_page_but_keeps_its_glyphs() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
    buffer[(0, 0)]
        .set_symbol("S")
        .set_fg(Color::Rgb(0xEE, 0xF3, 0xF1));
    super::chrome::dim_backdrop(&mut buffer, &DARK);
    assert_eq!(buffer[(0, 0)].symbol(), "S", "the glyph survives the fade");
    let Color::Rgb(r, g, b) = buffer[(0, 0)].fg else {
        panic!("the theme's ink is RGB");
    };
    assert!(
        r < 0xEE && g < 0xF3 && b < 0xF1,
        "the page's ink dims toward the canvas"
    );
    assert!(r > 0x0A, "the page still reads behind the dialog");
}

/// Opening a settings dialog runs the fade over the page, not just the
/// dialog's own box.
#[test]
fn a_settings_dialog_fades_the_page_behind_it() {
    let ink = |dialog: bool| {
        let mut settings = crate::screens::SettingsScreen::new();
        if dialog {
            settings.dialog = Some(crate::screens::AddInstanceDialog::new(&settings.stored));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(
                    frame,
                    &settings,
                    &DARK,
                    &crate::icons::UNICODE,
                    &mut Vec::new(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = (0..16)
            .find_map(|y| {
                let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                row.find("Settings").map(|x| (x as u16, y))
            })
            .expect("the sidebar heading draws");
        buffer[(x, y)].style().fg.expect("the heading has ink")
    };
    let plain = ink(false);
    let hazed = ink(true);
    let brightness = |color: Color| {
        let Color::Rgb(r, g, b) = color else {
            panic!("the theme's ink is RGB");
        };
        r as u32 + g as u32 + b as u32
    };
    assert!(
        brightness(hazed) < brightness(plain),
        "the page sits back under the dialog: {plain:?} -> {hazed:?}"
    );
}

#[test]
fn the_add_dialog_frame_and_title_read_on_both_grounds() {
    let inks = |theme: &crate::palette::Theme| {
        let mut settings = crate::screens::SettingsScreen::new();
        settings.dialog = Some(crate::screens::AddInstanceDialog::new(&settings.stored));
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(
                    frame,
                    &settings,
                    theme,
                    &crate::icons::UNICODE,
                    &mut Vec::new(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let y = (0..16)
            .find(|&y| {
                (0..80)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("Add a login")
            })
            .expect("the dialog title draws");
        let x = (0..80)
            .find(|&x| buffer[(x, y)].symbol() == "A")
            .expect("the title starts after the border");
        let corner = (0..80)
            .find(|&x| buffer[(x, y)].symbol() == "╭")
            .expect("the frame starts on the title row");
        (
            buffer[(x, y)].style().fg.expect("the title has ink"),
            buffer[(corner, y)].style().fg.expect("the frame has ink"),
        )
    };
    assert_eq!(inks(&DARK), (DARK.text, DARK.text3));
    let native = DARK.painted(crate::palette::Background::Native);
    assert_eq!(inks(&native), (DARK.text, DARK.text3));
}

#[test]
fn the_picker_row_badges_only_the_capabilities_a_model_declares() {
    let mut card = model("m");
    let row_text = |card: &ModelCard| {
        super::model_picker::model_row("google", card, &crate::icons::UNICODE, &DARK, 80)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect::<String>()
    };
    // A text-only model wears nothing, and neither does one nobody
    // described: the column badges what is there, it does not grade.
    assert!(!row_text(&card).contains('◉'), "{}", row_text(&card));

    card.supports_vision = Some(true);
    assert!(row_text(&card).contains('◉'), "{}", row_text(&card));

    card.supports_vision = Some(false);
    assert!(!row_text(&card).contains('◉'), "{}", row_text(&card));
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
    // Five toggles render, all values one aligned column over; the
    // longest label (`Background`, ten cells) keeps a space before its
    // value instead of swallowing it.
    // The screen's own placeholder state, nerd icons, native
    // background, the terminal's own caret, is what renders here; the
    // opener overwrites it.
    let row = |label: &str, value: &str| format!("{:<11}{value}", label);
    assert!(text.contains(&row("Background", "Native")), "{text}");
    assert!(text.contains(&row("Colors", "Rhema")), "{text}");
    assert!(text.contains(&row("Theme", "Dark")), "{text}");
    assert!(text.contains(&row("Icons", "Nerd")), "{text}");
    assert!(text.contains(&row("Cursor", "Default")), "{text}");
    assert!(!text.contains("BackgroundNative"), "{text}");
}

/// The provider removal confirm: titled, the question names the
/// instance, the answer is y over n, all over the dimmed page.
#[test]
fn the_instance_confirm_names_the_provider_and_its_answer() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.instance_confirm = Some("home".to_owned());
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
    assert!(text.contains(" Confirmation "), "{text}");
    assert!(
        text.contains("Are you sure you want to delete provider home?"),
        "{text}"
    );
    assert!(text.contains("y/n"), "{text}");
}

/// The web search provider's removal confirm: the same modal the
/// instance page uses, drawn so `d` does not leave the page looking
/// frozen while the confirm swallows the keys.
#[test]
fn the_websearch_confirm_names_the_provider_and_its_answer() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.websearch_confirm = Some("exa-work".to_owned());
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
    assert!(text.contains(" Confirmation "), "{text}");
    assert!(
        text.contains("Are you sure you want to delete provider exa-work?"),
        "{text}"
    );
    assert!(text.contains("y/n"), "{text}");
}

/// The profile's removal confirm: the same modal, naming the profile.
#[test]
fn the_profile_confirm_names_the_profile_and_its_answer() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = crate::screens::SettingsSection::Profiles;
    settings.profile_confirm = Some("work".to_owned());
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
    assert!(text.contains(" Confirmation "), "{text}");
    assert!(
        text.contains("Are you sure you want to delete profile work?"),
        "{text}"
    );
    assert!(text.contains("y/n"), "{text}");
}

/// The Update page: this build's row, the channel's latest with its
/// verdict, and the one install row, dim until a check earned it. The
/// check row is gone; the startup check, `/update` and `rhema update`
/// are how it is asked again, and the page says so.
#[test]
fn the_update_page_shows_the_versions_and_its_install_row() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Update;
    settings.update.check =
        crate::screens::UpdateCheck::Done(Ok(crate::update::current_version().to_owned()));
    let drawn = |settings: &crate::screens::SettingsScreen| {
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        (0..40)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<String>()
    };
    let text = drawn(&settings);
    assert!(text.contains("Current"), "{text}");
    assert!(
        text.contains("up to date"),
        "the channel at this build is no update: {text}"
    );
    assert!(
        !text.contains("Check for updates"),
        "the page carries no check row: {text}"
    );
    assert!(text.contains("Update now"), "{text}");
    assert!(
        text.contains("/update asks again"),
        "the page names the way to ask again: {text}"
    );
    assert!(
        !text.contains("downloading and installing"),
        "an idle install says nothing: {text}"
    );

    // A newer release: the verdict flips, the ask-again hint goes, and
    // the install run's outcome renders under the row.
    settings.update.check = crate::screens::UpdateCheck::Done(Ok("99.0.0".to_owned()));
    settings.update.install = crate::screens::UpdateInstall::Running;
    let text = drawn(&settings);
    assert!(text.contains("update available"), "{text}");
    assert!(
        !text.contains("/update asks again"),
        "an available release needs no hint: {text}"
    );
    assert!(text.contains("downloading and installing"), "{text}");

    // A failed run names itself, not success.
    settings.update.install =
        crate::screens::UpdateInstall::Done(Err("checksum mismatch".to_owned()));
    let text = drawn(&settings);
    assert!(
        text.contains("the install failed: checksum mismatch"),
        "{text}"
    );
}

/// The Usage page's detailed split: the note that names the three
/// counts, the total, and the six rows as percentages of it.
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
    let mut terminal = Terminal::new(TestBackend::new(84, 40)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text: String = (0..40)
        .map(|y| {
            (0..84)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(text.contains("Detailed Session Usage"), "{text}");
    // The note names each of the three counts once, so the rows stay
    // three figures wide.
    assert!(
        text.contains("what one request carried on average"),
        "{text}"
    );
    assert!(text.contains("counted once at about four"), "{text}");
    assert!(text.contains("requests; 78% cached"), "{text}");
    // Every row carries all three counts, the total included.
    assert!(
        text.contains("10k sent \u{b7} ~94.5k log \u{b7} 100k billed"),
        "{text}"
    );
    assert!(
        text.contains("20% 2k sent \u{b7} no log \u{b7} 20k billed"),
        "{text}"
    );
    assert!(
        text.contains("2% 200 sent \u{b7} no log \u{b7} 2k billed"),
        "{text}"
    );
    assert!(
        text.contains("30% 3k sent \u{b7} ~2.5k log \u{b7} 30k billed"),
        "{text}"
    );
    assert!(
        text.contains("30% 3k sent \u{b7} ~90k log \u{b7} 30k billed"),
        "{text}"
    );
    assert!(
        text.contains("8% 800 sent \u{b7} ~800 log \u{b7} 8k billed"),
        "{text}"
    );
    assert!(
        text.contains("10% 1k sent \u{b7} ~1.2k log \u{b7} 10k billed"),
        "{text}"
    );
}

/// The Usage page's view schema: a bar of the block against the band,
/// then the cut, the digest count, and what the band holds. Each state
/// says what it is instead of drawing an empty bar.
#[test]
fn the_usage_page_shows_the_views_shape() {
    let draw = |settings: &crate::screens::SettingsScreen| {
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(84, 40)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        (0..40)
            .map(|y| {
                (0..84)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<String>()
    };
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Usage;
    settings.transcript = Some(rhema_transcript::TranscriptStats {
        requests: 10,
        pass_through: 1,
        viewed: 9,
        sections: 101,
        sectioned_messages: 1_397,
        log_messages: 1_859,
        block_bytes: 19_400,
        block_tokens: 4_850,
        band_bytes: 2_530_000,
        band_tokens: 632_500,
        tail_bytes: 6_400,
        tail_tokens: 1_600,
        ..Default::default()
    });
    let text = draw(&settings);
    // 19.4 KB of a 2.55 MB view is under one percent: the bar keeps a
    // cell for it and the label carries the figure.
    assert!(text.contains("0.8% block"), "{text}");
    assert!(text.contains('\u{2588}'), "the block's cells: {text}");
    assert!(text.contains('\u{2591}'), "the band's cells: {text}");
    assert!(
        text.contains("1397 of 1859 messages · 101 digests"),
        "{text}"
    );
    // The Band row wraps before its last word at 80 columns, the
    // sidebar taking its nineteen; the figure stays on the first line.
    assert!(
        text.contains("~632.5k carried whole · ~1.6k this"),
        "{text}"
    );
    // The counters spell the block in tokens too; the row wraps
    // between the word and the figure at this width.
    assert!(text.contains("~4.8k tokens"), "{text}");

    // Below the budget no block exists, so the page says where the log
    // went instead of drawing an empty bar.
    settings.transcript = Some(rhema_transcript::TranscriptStats {
        requests: 5,
        pass_through: 5,
        ..Default::default()
    });
    let text = draw(&settings);
    // The row wraps at the pane's width, so the note is matched in
    // pieces.
    assert!(
        text.contains("under budget · 5 requests rode the log"),
        "{text}"
    );
    assert!(text.contains("whole"), "{text}");

    settings.transcript = Some(rhema_transcript::TranscriptStats::default());
    let text = draw(&settings);
    assert!(text.contains("nothing measured yet"), "{text}");
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
            api: None,
        },
    );
    settings.stored.provider = Some("google".into());
    settings.stored.instance = Some("google".into());

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let rows: Vec<String> = (0..18)
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
    for section in settings.sections() {
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
        settings.sections().len() + 2,
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
        settings.sections().len() + 2,
        "sidebar rows and the two page rows register: {hits:?}"
    );
}

/// A long description renders at most two rows, both indented, the
/// second ending in an ellipsis, never a sprawl from column zero.
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

/// The Skills page caps its descriptions at two indented rows,
/// the same discipline the Subagents page applies, and the license
/// renders one dim row under them.
#[test]
fn the_skills_page_caps_the_description_and_dims_the_license() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Skills;
    settings.skills_global_dir = Some("/cfg".into());
    settings.skills_workspace_dir = "/w/.rhema/skills".into();
    settings.skills = vec![crate::screens::SkillRow {
        name: "wordy".into(),
        description: "Takes a long task and breaks it into pieces, reports each one \
             as it lands, and keeps a running summary so the parent can follow \
             along without waiting for the end of the whole run to conclude."
            .into(),
        license: Some("MIT License, Copyright (c) 2026 Someone Somewhere".into()),
        source: crate::screens::SkillSource::Workspace,
        enabled: true,
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
        .position(|row| row.contains("wordy · workspace"))
        .expect("the name row renders");
    let name_col = rows[name_row]
        .char_indices()
        .position(|(_, ch)| ch == 'w')
        .expect("the name");
    let indent = |row: &String, col: usize| {
        row.chars()
            .skip(col.saturating_sub(4))
            .take(4)
            .all(char::is_whitespace)
    };
    let first = &rows[name_row + 1];
    let second = &rows[name_row + 2];
    let license = &rows[name_row + 3];
    let after = &rows[name_row + 4];
    assert!(indent(first, name_col), "the first row indents: {first:?}");
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
        !after.contains("whole run"),
        "the tail is dropped, no third row: {after:?}"
    );
    assert!(
        license.contains("MIT License"),
        "the license renders: {license:?}"
    );
    let dim = (0..60).any(|x| {
        terminal.backend().buffer()[(x, (name_row + 3) as u16)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::DIM)
    });
    assert!(dim, "the license renders dimmed: {license:?}");
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
        settings.sections().len() + 3,
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
        rhema_config::Settings::default().profiles,
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
    assert!(text.contains("This step by step"), "{text}");
    assert!(text.contains("Press Enter"), "{text}");
    assert!(text.contains("later in /settings"));
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
        rhema_config::Settings::default().profiles,
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

/// The rail reads as progress, not a menu: passed steps carry a check,
/// the current one is a filled dot, and the ones ahead are hollow.
#[test]
fn the_rail_checks_passed_steps_and_marks_the_current_one() {
    let mut wizard = crate::onboarding::OnboardingScreen::new(
        Vec::new(),
        "personal".into(),
        rhema_config::Settings::default().profiles,
        false,
        rhema_config::Profile::default(),
    );
    wizard.go(crate::onboarding::OnboardingStep::Login);
    let mut logo_rect = None;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
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
    let text: String = (0..24)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Step 3 of 7"), "{text}");
    assert!(text.contains("✓ Welcome"), "{text}");
    assert!(text.contains("✓ Profile"), "{text}");
    assert!(text.contains("● Provider"), "{text}");
    assert!(text.contains("○ Model"), "{text}");
    let rail: String = (0..24)
        .map(|y| {
            (0..26)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rail.contains("❯"), "no cursor arrow in the rail: {rail}");
}

/// The wizard opens on the profile choice: the config's profiles with
/// their stored provider, the default marked, and the add row.
#[test]
fn the_wizard_s_profile_step_lists_the_registry_and_the_add_row() {
    let profiles = rhema_config::Settings::default().profiles;
    let mut wizard = crate::onboarding::OnboardingScreen::new(
        Vec::new(),
        "personal".into(),
        profiles,
        false,
        rhema_config::Profile::default(),
    );
    wizard.go(crate::onboarding::OnboardingStep::Profile);
    let mut logo_rect = None;
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
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
    let text: String = (0..24)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("The default profile"), "{text}");
    assert!(text.contains("personal"), "{text}");
    assert!(text.contains("work"), "{text}");
    assert!(text.contains("default"), "{text}");
    assert!(text.contains("+ Add a new profile..."), "{text}");
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

/// The add dialog opens on the categories, and a category opens its
/// provider list. Both levels register click targets.
#[test]
fn the_add_dialog_starts_with_categories_then_lists_providers() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.dialog = Some(crate::screens::AddInstanceDialog::new(&settings.stored));

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let rows: Vec<String> = (0..18)
        .map(|y| {
            (0..80)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect()
        })
        .collect();
    let text = rows.join("\n");
    assert!(text.contains("Add a login"), "the dialog title: {text}");
    assert!(
        text.contains("Subscriptions")
            && text.contains("API / Pay-as-you-go")
            && text.contains("Local/Custom"),
        "the category list renders: {text}"
    );
    assert!(
        !text.contains("Google Gemini"),
        "providers wait behind their category: {text}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.trim_end().ends_with(" providers")),
        "the category rows carry no count: {text}"
    );
    let category_hits = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, SettingsHit::DialogCategory(_)))
        .count();
    assert_eq!(
        category_hits,
        crate::screens::ProviderCategory::ALL.len(),
        "each category is clickable"
    );

    // Open the API category: its providers draw and register.
    settings.dialog.as_mut().unwrap().select_category_row(1);
    settings.dialog.as_mut().unwrap().choose_category();
    let mut hits = Vec::new();
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
    assert!(
        text.contains("Google Gemini") && text.contains("Anthropic"),
        "the provider list renders: {text}"
    );
    assert!(
        !text.contains("Local/Custom"),
        "the category list is behind the providers: {text}"
    );
    let provider_hits = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, SettingsHit::DialogProvider(_)))
        .count();
    assert_eq!(
        provider_hits,
        crate::screens::providers_in(crate::screens::ProviderCategory::Api).len(),
        "each provider is clickable"
    );
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

/// A provider whose fetch timed out is painted as unavailable, in the
/// error color, next to the live catalogs.
#[test]
fn the_footer_paints_unavailable_providers_in_red() {
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
    picker.unavailable = vec!["llama-cpp"];

    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    terminal
        .draw(|frame| render_picker(frame, &picker, &crate::icons::UNICODE, &DARK))
        .unwrap();
    let footer: String = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 10)].symbol())
        .collect();
    assert!(footer.contains("unavailable: llama-cpp"), "{footer}");
    let x = footer.find("unavailable").expect("the footer names it") as u16;
    assert_eq!(
        terminal.backend().buffer()[(x, 10)].style().fg,
        Some(DARK.error),
        "{footer}"
    );
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

/// Wide characters are two cells each however many characters they
/// are: the fit that passes a line through verbatim measures cells,
/// or a CJK line walks past the terminal edge with no wrapped row
/// beneath it.
#[test]
fn wrap_passes_a_line_through_only_when_it_fits_in_cells() {
    let wide: String = "中".repeat(45);
    let line = Line::from(Span::raw(wide.clone()));
    let rows = wrap_line(&line, 60);
    assert_eq!(rows.len(), 2, "90 cells at width 60 wrap to 2 rows");
    let joined: String = rows
        .iter()
        .flat_map(|row| row.iter().map(|span| span.content.clone()))
        .collect();
    assert_eq!(joined, wide);
    let width: usize = rows[0].iter().map(|span| span.content.width()).sum();
    assert!(width <= 60, "no row exceeds the width: {width}");
    // A line that fits in cells still passes through in one piece.
    let fits = Line::from(Span::raw("中".repeat(30)));
    assert_eq!(wrap_line(&fits, 60).len(), 1);
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

/// The flow stage's link is a pair of buttons, not a wrapped URL:
/// the full query string never reaches the screen, the host does,
/// and each button has its own click target.
#[test]
fn the_flow_stage_renders_open_and_copy_buttons() {
    let mut dialog = crate::screens::AddInstanceDialog::new(&rhema_config::Profile::default());
    dialog.stage = crate::screens::DialogStage::Flow;
    dialog.flow_instruction = Some(rhema_provider::LoginInstruction {
        text: "sign in with ChatGPT".into(),
        url: Some(
            "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_long".into(),
        ),
        code: None,
    });
    dialog.flow_waiting = Some("waiting for the browser".into());
    dialog.flow_notice = Some("link copied to the clipboard".into());
    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            crate::ui::dialogs::render_add_dialog(frame, &dialog, &crate::palette::DARK, &mut hits)
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
        .collect();
    assert!(screen.contains("[ enter open in browser ]"), "{screen}");
    assert!(screen.contains("[ c copy link ]"), "{screen}");
    assert!(screen.contains("link: auth.openai.com"), "{screen}");
    assert!(screen.contains("link copied to the clipboard"), "{screen}");
    assert!(
        !screen.contains("client_id"),
        "the full URL stays off the screen: {screen}"
    );
    let open = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, crate::app::SettingsHit::DialogFlowOpen))
        .count();
    let copy = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, crate::app::SettingsHit::DialogFlowCopy))
        .count();
    assert_eq!((open, copy), (1, 1), "one target per button: {hits:?}");
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
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn chat_app() -> App {
        app_with_home(Some(
            std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id())),
        ))
    }

    fn app_with_home(home: Option<PathBuf>) -> App {
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
                api: None,
            },
        );
        let config = crate::store::Config {
            settings: rhema_config::Settings {
                profiles: BTreeMap::from([("personal".to_owned(), personal)]),
                ..rhema_config::Settings::default()
            },
            ..Default::default()
        };
        let mut app = App::new(AppSetup {
            store: Arc::new(MemoryStore::default()),
            checkpoint: None,
            sessions_base: None,
            config,
            backend: Some(crate::app::Backend::Google {
                api_key: "k".into(),
            }),
            session: rhema_core::Session::default(),
            secrets: None,
            sandboxed: None,
            sandbox_machines: crate::sandbox::SandboxMachines::default(),
            home,
            onboard: false,
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: ThemeKind::Dark,
                background: crate::palette::Background::Rhema,
                m3_source: None,
                custom: None,
                icons: crate::icons::Flavor::Unicode,
                cursor: crate::cursor::CursorStyle::Default,
                scrollbar: true,
                thinking: rhema_core::request::ThinkingEffort::Default,
                profile: Some("personal".into()),
                workspace: PathBuf::from("."),
                max_steps: NonZeroU32::new(4).unwrap(),
                no_transcript: false,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        // `App::new` probes the host for missing tools, and the strip
        // naming them rides the transcript canvas under the banner. On
        // a machine without the probed tools the strip wraps into rows
        // that fill this harness's 12-row viewport, and every screen
        // assertion here answers for the strip, not the transcript.
        // The harness pins an empty probe: hermetic across hosts; the
        // strip's own test sets `missing_tools` back by hand.
        app.missing_tools.clear();
        app
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

    /// The startup tools strip rides the transcript header: one row
    /// under the banner naming the probed tools this machine misses, and
    /// the ask that installs them. It shows once with the banner and
    /// scrolls away as the conversation grows; the prompt's environment
    /// section carries the same facts from the first turn on.
    #[test]
    fn the_missing_tools_strip_sits_under_the_banner() {
        let mut app = chat_app();
        app.missing_tools = vec!["fd".into(), "jq".into()];
        // A wide frame: the strip's whole sentence must fit, where the
        // shared 40-column harness would clip it mid-clause.
        let rows = {
            let backend = TestBackend::new(90, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let banner_row = rows
            .iter()
            .position(|row| row.contains(crate::VERSION_LABEL))
            .expect("the banner draws");
        assert!(
            rows[banner_row + 2].contains("missing tools: fd, jq"),
            "the strip sits under the banner's blank row: {rows:?}"
        );
        assert!(
            rows[banner_row + 2].contains("ask the agent to install them"),
            "the strip names the ask: {rows:?}"
        );
    }

    /// The startup check's answer rides the banner: one line under the
    /// version label naming the three ways to install, gone once the
    /// install landed.
    #[test]
    fn the_banner_announces_an_available_update() {
        let mut app = chat_app();
        app.update.check = crate::screens::UpdateCheck::Done(Ok("99.0.0".to_owned()));
        let rows_at = |app: &mut App, width: u16| {
            let backend = TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let rows = rows_at(&mut app, 110);
        let banner = rows
            .iter()
            .position(|row| row.contains(crate::VERSION_LABEL))
            .expect("the banner draws");
        assert!(
            rows[banner + 1].contains("A new update is available."),
            "the notice sits right under the version: {rows:?}"
        );
        assert!(
            rows[banner + 1].contains("/update") && rows[banner + 1].contains("rhema update"),
            "the notice carries both commands: {rows:?}"
        );
        assert!(
            rows[banner + 1].contains("Settings -> Update"),
            "the notice names the page: {rows:?}"
        );
        assert!(
            rows[banner + 2].trim().is_empty(),
            "the banner's blank row follows the notice: {rows:?}"
        );

        // The install landed: the new version is on disk and waiting for
        // a restart, so the banner stops asking.
        app.update.install =
            crate::screens::UpdateInstall::Done(Ok("installed rhema 99.0.0".to_owned()));
        let rows = rows_at(&mut app, 110);
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("A new update is available.")),
            "a landed install retires the ask: {rows:?}"
        );
    }

    /// A background shell pins one line under the spinner: its badge, the
    /// command as the model wrote it, and how long this process has
    /// watched it.
    #[test]
    fn a_running_job_pins_a_line_with_its_command_and_age() {
        let mut app = chat_app();
        app.jobs = Some(crate::jobs::JobsHandle::stub(vec![crate::jobs::JobEntry {
            row: rhema_bash::jobs::JobRow {
                id: "abc".to_owned(),
                status: "running".to_owned(),
                exit_code: None,
                command: "npm run dev".to_owned(),
                session: Some("s1".to_owned()),
                started_at: None,
            },
            age: std::time::Duration::from_secs(65),
        }]));
        let text = screen_text(&mut app);
        assert!(text.contains("npm run dev"), "{text}");
        assert!(text.contains("1min 5s"), "{text}");
    }

    /// A selection reverses exactly the cells its text covers: the
    /// painter reads the same row strings the copy does, so the two
    /// cannot disagree.
    #[test]
    fn a_selection_reverses_the_cells_it_covers() {
        use crate::app::selection::Selection;
        use ratatui::style::Modifier;

        let mut app = chat_app();
        app.transcript.push_notice("hello world");
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();

        let area = app.transcript_view.area;
        let row = app
            .transcript_view
            .rows
            .iter()
            .position(|row| row.contains("hello"))
            .expect("the notice is drawn");
        let start_col = app.transcript_view.rows[row].find("hello").unwrap() as u16;
        let mut selection = Selection::new(
            (area.x + start_col, area.y + row as u16),
            app.transcript_view.top,
        );
        selection.extend((area.x + start_col + 5, area.y + row as u16));
        app.selection = Some(selection);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();

        let y = area.y + row as u16;
        let reversed = |x: u16| {
            terminal.backend().buffer()[(x, y)]
                .modifier
                .contains(Modifier::REVERSED)
        };
        for x in area.x + start_col..area.x + start_col + 5 {
            assert!(reversed(x), "cell {x} carries the highlight");
        }
        assert!(
            !reversed(area.x + start_col + 5),
            "the cell after the selection stays plain"
        );
    }

    /// While the model streams, rows land below the selection. The
    /// viewport stays on the row the drag started from, so the highlight
    /// keeps covering the same words and the copy reads them back.
    #[test]
    fn streaming_output_does_not_move_a_selection() {
        use crate::app::selection::{self, Selection};

        let mut app = chat_app();
        app.transcript
            .push_user(&format!("{} hello", "word ".repeat(200)), Vec::new());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();

        let area = app.transcript_view.area;
        let row = app
            .transcript_view
            .rows
            .iter()
            .rposition(|row| row.contains("hello"))
            .expect("the line is drawn");
        let byte = app.transcript_view.rows[row].rfind("hello").unwrap();
        let col =
            unicode_width::UnicodeWidthStr::width(&app.transcript_view.rows[row][..byte]) as u16;
        let mut selection =
            Selection::new((area.x + col, area.y + row as u16), app.transcript_view.top);
        selection.extend((area.x + col + 5, area.y + row as u16));
        app.selection = Some(selection);
        let pinned = selection.viewport_top;

        // The model keeps writing after the selection lands.
        app.transcript.push_notice("still writing");
        terminal.draw(|frame| render(&mut app, frame)).unwrap();

        assert_eq!(app.transcript_view.top, pinned, "the window stayed put");
        assert_eq!(
            selection::text(&app.transcript_view, selection),
            "hello",
            "the drag still covers the word it started on"
        );
        assert!(
            app.transcript_view.rail.is_some(),
            "the pinned window reads as history"
        );

        // Clearing the selection leaves the view where the user saw it;
        // the new output waits below until they scroll to the tail.
        app.selection = None;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.transcript_view.top, pinned, "the window stays put");
        assert!(
            !app.transcript_view
                .rows
                .iter()
                .any(|row| row.contains("still writing")),
            "the new output is below the frozen window"
        );
        app.chat.scroll = 0;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(
            app.transcript_view
                .rows
                .iter()
                .any(|row| row.contains("still writing")),
            "the new output waits at the tail"
        );
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
    /// line leaves at once, the transcript's finish notice carries the
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
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
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
            let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
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
        assert!(screen.contains("general-purpose, explore"), "{screen}");
        assert!(screen.contains("✓"), "{screen}");
    }

    /// A zoom call's row shows the range it opened.
    #[test]
    fn a_zoom_call_renders_its_arguments() {
        let mut app = chat_app();
        app.transcript.apply(&OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "zoom".into(),
        });
        app.transcript.apply(&OwnedEvent::ToolCallArgsDelta {
            id: "c1".into(),
            delta: r#"{"id": 3, "n": 2}"#.into(),
        });
        app.transcript.apply(&OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "3+2|the third message".into(),
            is_error: false,
            code: None,
        });
        let screen = screen_text(&mut app);
        assert!(screen.contains("zoom(3, 2)"), "{screen}");
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
                api: None,
            },
        );
        settings.stored.instances.insert(
            "work sub".to_owned(),
            rhema_config::ProviderInstance {
                provider: "copilot".into(),
                api_key: Some("g".into()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        settings.stored.provider = Some("copilot".into());
        settings.stored.instance = Some("work sub".into());

        let backend = TestBackend::new(60, 24);
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
            if let Some(pos) = row.find("pick the model") {
                columns.push(("pick the model", pos));
            }
            if let Some(pos) = row.find("reasoning effort") {
                columns.push(("reasoning effort", pos));
            }
        }
        assert_eq!(columns.len(), 2, "all searched rows visible: {columns:?}");
        assert!(
            columns.iter().all(|(_, pos)| *pos == columns[0].1),
            "descriptions misaligned: {columns:?}"
        );
    }

    /// The footer's home is the environment's resolution, not `HOME`
    /// alone: windows keeps `HOME` unset and names the home in
    /// `USERPROFILE`, so the trim must read that second name too.
    #[test]
    fn the_footer_home_reads_the_environment_resolution() {
        let app = app_with_home(None);
        assert_eq!(app.home, rhema_config::user_home().ok());
    }

    /// The footer's folder: the home and its `Documents` trim away, and
    /// at most two parents ride beside the folder itself, three
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

    /// The windows spelling trims by the same rule: the separators
    /// fold, the drive component never shows, and the verbatim prefix
    /// a windows canonicalize adds has been stripped upstream, the
    /// footer spells '/' whatever the host.
    #[test]
    fn the_footer_trims_a_windows_path_the_same_way() {
        let home = Path::new(r"C:\Users\u");
        assert_eq!(
            footer_folder(Path::new(r"C:\Users\u\Documents\prjs\rhema"), Some(home)),
            (String::new(), vec!["prjs".to_owned()], "rhema".to_owned())
        );
        // Outside the home: absolute, so the slash prefix rides, and
        // the drive letter stays out of the parents.
        assert_eq!(
            footer_folder(Path::new(r"D:\work\foo"), Some(home)),
            ("/".to_owned(), vec!["work".to_owned()], "foo".to_owned())
        );
    }

    /// The window title: the mark, then the workspace as the footer
    /// spells it, the home trim, the capped parents. No badge glyph:
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

    /// The footer names where bash runs only when that is not the
    /// host: a local session spends no segment on it, and a running
    /// sandbox shows its kind, or its instance's name when the
    /// profile's active pointer names it.
    #[test]
    fn the_footer_names_where_bash_runs() {
        let mut app = chat_app();
        app.config.profiles.get_mut("personal").unwrap().provider = Some("google".to_owned());
        assert!(
            !screen_text(&mut app).contains("▣"),
            "the local fast path is no badge: {}",
            screen_text(&mut app)
        );
        app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: crate::args::SandboxChoice::Docker,
            target: "rhema-dev".to_owned(),
            tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
            shared: None,
            mcp: crate::mcp_config::McpStore::local(None, &Default::default()),
            session: Default::default(),
            sandbox: None,
            remote: None,
            remote_session: Default::default(),
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
    /// money amount rides bare, the icon says the unit, while a
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
        // Wide enough for every segment at once, the badge and the
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
                family: None,
                release_date: None,
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

        // Scrolled far back, far past the top, even, the draw clamps
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
        app.transcript.push_user_with(
            &format!("look at {long} this"),
            Vec::new(),
            vec![crate::paste::PasteSpan {
                start: "look at ".len(),
                end: "look at ".len() + long.len(),
                lines: 5,
            }],
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

    /// A running command's output earns the preview at a second line,
    /// an instant command never flashes, or at a second of runtime,
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
            remaining: 0,
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
                    family: None,
                    release_date: None,
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
        // Twenty-one commands, six visible rows: twelve downs put
        // the selection on /retry, and the window must have followed
        // it.
        for _ in 0..12 {
            app.chat
                .handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
        }
        assert_eq!(app.chat.palette_index, 12);
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
        assert!(
            !screen.contains("/model"),
            "the head scrolled out: {screen}"
        );

        // And back to the top restores it.
        app.chat.palette_index = 0;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(screen.contains("/model"), "{screen}");
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
    fn the_cursor_counts_wide_characters_in_cells() {
        // Each wide character is one step of the source but two cells
        // of the render: the caret lands where they draw, not a step
        // short per character.
        let layout = input_layout("中文abc", &[], 40, &DARK);
        assert_eq!(cursor_at_rows(&layout, 0), Some((0, 0)));
        assert_eq!(cursor_at_rows(&layout, 1), Some((0, 2)), "after 中");
        assert_eq!(cursor_at_rows(&layout, 2), Some((0, 4)), "after 中文");
        assert_eq!(cursor_at_rows(&layout, 5), Some((0, 7)), "at the end");

        // The wide text also widens the column a chip's cursor math
        // works from.
        let marked = format!("中{}a", ChatState::MARK);
        let layout = input_layout(&marked, &[Chip::Paste("x\ny".to_owned())], 40, &DARK);
        assert_eq!(cursor_at_rows(&layout, 1), Some((0, 2)), "on the chip");
        assert_eq!(
            cursor_at_rows(&layout, 2),
            Some((0, 9)),
            "the chip crossed whole after two cells of text"
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
    /// their source characters stay counted, so every cursor offset,
    /// the tail's included, lands on a row and the input renders past
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

        // One PageDown now moves the visible window by the whole tick,
        // no stranded surplus to burn first.
        app.chat.scroll_by(-3);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.chat.scroll, clamped - 3);
    }

    #[test]
    fn the_scroll_rail_shows_only_while_viewing_history() {
        let mut app = chat_app();
        // The banner alone fits the harness's viewport: no rail, and
        // its column stays blank.
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(app.transcript_view.rail.is_none());
        let buffer = terminal.backend().buffer();
        assert!(
            (0..buffer.area.height).all(|y| buffer[(38, y)].symbol() != "▐"),
            "no rail on a fitting transcript"
        );

        // A wall of text overflows, but the live tail is the default
        // reading position: the rail stays off there.
        app.transcript
            .push_user(&format!("{} end", "word ".repeat(200)), Vec::new());
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(app.transcript_view.rail.is_none(), "off at the tail");
        let buffer = terminal.backend().buffer();
        assert!(
            (0..buffer.area.height).all(|y| buffer[(38, y)].symbol() != "▐"),
            "no rail at the tail"
        );

        // One step into the history brings it in, on the last margin
        // column and running the transcript's whole height.
        app.chat.scroll = 1;
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let rail = app.transcript_view.rail.expect("the rail draws in history");
        assert_eq!(rail.area.x, 38, "flush with the right margin");
        assert!(rail.total > rail.visible);
        let buffer = terminal.backend().buffer();
        let cells: Vec<&str> = (rail.area.y..rail.area.y + rail.area.height)
            .map(|y| buffer[(rail.area.x, y)].symbol())
            .collect();
        assert!(
            cells.iter().all(|symbol| *symbol == "▐"),
            "the track draws: {cells:?}"
        );
    }

    #[test]
    fn the_scrollbar_toggle_keeps_the_rail_off() {
        let mut app = chat_app();
        app.transcript
            .push_user(&format!("{} end", "word ".repeat(200)), Vec::new());
        app.chat.scroll = 1;
        app.options.scrollbar = false;
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(app.transcript_view.rail.is_none());
        let buffer = terminal.backend().buffer();
        assert!(
            (0..buffer.area.height).all(|y| buffer[(38, y)].symbol() != "▐"),
            "the margin stays bare with the scrollbar off"
        );
    }

    #[test]
    fn the_rail_thumb_follows_the_visible_window() {
        let mut app = chat_app();
        app.transcript
            .push_user(&format!("{} end", "word ".repeat(300)), Vec::new());
        let draw = |app: &mut App| {
            let backend = TestBackend::new(40, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(app, frame)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (app.transcript_view.rail, buffer)
        };
        let thumb_rows = |buffer: &ratatui::buffer::Buffer,
                          rail: &crate::app::selection::ScrollRail| {
            (rail.area.y..rail.area.y + rail.area.height)
                .filter(|y| buffer[(rail.area.x, *y)].style().fg == Some(DARK.text3))
                .collect::<Vec<_>>()
        };
        // Just into the history, the thumb sits low on the track.
        app.chat.scroll = 1;
        let (rail, buffer) = draw(&mut app);
        let rail = rail.expect("the rail draws in history");
        assert!(
            thumb_rows(&buffer, &rail).first().copied() > Some(rail.area.y + 1),
            "the thumb starts low"
        );
        // At the head, it reaches the top.
        app.chat.scroll = 1000;
        let (rail, buffer) = draw(&mut app);
        let rail = rail.expect("the rail draws in history");
        assert_eq!(
            thumb_rows(&buffer, &rail).first().copied(),
            Some(rail.area.y),
            "the thumb reaches the head"
        );
        // A drag that reaches the tail keeps the rail on until release,
        // the thumb on the track's bottom.
        app.chat.scroll = 0;
        app.rail_drag = true;
        let (rail, buffer) = draw(&mut app);
        let rail = rail.expect("the rail survives the drag");
        assert_eq!(
            thumb_rows(&buffer, &rail).last().copied(),
            Some(rail.area.y + rail.area.height - 1),
            "the thumb ends at the tail"
        );
        app.rail_drag = false;
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
            .map(|row| row.trim().to_owned())
            .collect();
        assert!(
            rows.contains(&"╭───────╮".to_owned()),
            "a rounded frame opens the bubble: {rows:?}"
        );
        assert!(
            rows.contains(&"│ alpha │".to_owned())
                && rows.contains(&"│ beta  │".to_owned())
                && rows.contains(&"│ gamma │".to_owned()),
            "each source line keeps its own row: {rows:?}"
        );
        assert!(
            rows.contains(&"╰───────╯".to_owned()),
            "the frame closes under the last line: {rows:?}"
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
        // The whole prompt is visible, tail included, nothing clips.
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
        // reserve a one-row box and clip its tail, cursor and text
        // hidden below, until further typing pushed past the height
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
        // Flush right: one cell follows the code, the pane's right
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
        assert!(before.contains("investigate"), "{before}");
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
        assert!(after.contains("investigate"), "{after}");
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
        let alpha_bottom = (alpha_row..buffer.area.height)
            .find(|y| row_text(*y).contains('╰'))
            .expect("the bubble closes");
        let beta_row = (alpha_bottom..buffer.area.height)
            .find(|y| row_text(*y).contains("beta reply"))
            .expect("reply visible");
        let gamma_top = (beta_row..buffer.area.height)
            .find(|y| row_text(*y).contains('╭'))
            .expect("second prompt visible");
        // Every block breathes: one blank row under the bubble, one
        // after the reply.
        assert_eq!(beta_row, alpha_bottom + 2, "a blank row after the bubble");
        assert_eq!(gamma_top, beta_row + 2, "one blank row after the reply");
    }

    #[test]
    fn a_prompt_renders_as_a_right_aligned_bubble() {
        let mut app = chat_app();
        // One unbroken run wraps at the bubble's content width, so the
        // widest row fills the cap exactly.
        app.transcript.push_user(&"x".repeat(160), Vec::new());

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let rows: Vec<String> = (0..buffer.area.height).map(row_text).collect();
        let top = rows
            .iter()
            .position(|row| row.contains('╭'))
            .expect("the frame opens");
        let start = rows[top].chars().position(|c| c == '╭').unwrap();
        let end = rows[top].chars().position(|c| c == '╮').unwrap();
        // The bubble hugs the transcript's right edge: 56 cells wide,
        // drawn one cell in from the pane's left edge.
        assert_eq!(end + 1, 57, "flush right: {:?}", rows[top]);
        assert!(
            rows[top][..start].trim().is_empty(),
            "only the right-aligned pad leads: {:?}",
            rows[top]
        );
        // A long prompt stops at three quarters of the column.
        let bubble_width = end - start + 1;
        assert_eq!(bubble_width, 56 * 3 / 4, "capped at 3/4");
        // Nothing carries a fill of its own: the bubble is ink on the
        // page, every cell, text, frame, pads, on the well.
        let body_row = top + 1;
        for x in start as u16..57 {
            assert_eq!(
                buffer[(x, body_row as u16)].style().bg,
                Some(DARK.sunken),
                "cell {x} of the body sits on the well"
            );
        }
        assert_eq!(
            buffer[(start as u16, top as u16)].style().bg,
            Some(DARK.sunken),
            "the ╭ sits on the well"
        );
    }

    /// `background: native` hands the page fills to the terminal, and
    /// the bubble keeps them handed: every cell stays on the
    /// terminal's own ground, the reader's words framed but not
    /// papered over, the card is a `background: rhema` device.
    #[test]
    fn a_prompt_bubble_keeps_the_ground_under_a_native_background() {
        let mut app = chat_app();
        app.options.background = crate::palette::Background::Native;
        app.theme = app.options.resolved_theme();
        app.transcript.push_user("hi", Vec::new());
        assert!(app.theme.is_native());

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let top = (0..buffer.area.height)
            .find(|y| row_text(*y).contains('╭'))
            .expect("the frame opens");
        let start = row_text(top).chars().position(|c| c == '╭').unwrap() as u16;
        // The body, the frame, and the page around them: all Reset.
        let body = &buffer[(1 + start + 1, top + 1)];
        assert_eq!(
            body.style().bg,
            Some(ratatui::style::Color::Reset),
            "the bubble keeps the terminal's ground"
        );
        assert_eq!(
            buffer[(start, top)].style().bg,
            Some(ratatui::style::Color::Reset),
            "the ╭ is not painted"
        );
        assert_eq!(
            buffer[(1, top)].style().bg,
            Some(ratatui::style::Color::Reset),
            "the page stays the terminal's own"
        );
    }

    #[test]
    fn a_short_prompt_bubble_hugs_its_text() {
        let mut app = chat_app();
        app.transcript.push_user("hi", Vec::new());

        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let rows: Vec<String> = (0..buffer.area.height).map(row_text).collect();
        let top = rows
            .iter()
            .position(|row| row.contains('╭'))
            .expect("the frame opens");
        let start = rows[top].chars().position(|c| c == '╭').unwrap();
        let end = rows[top].chars().position(|c| c == '╮').unwrap();
        // Two content cells and the frame's four: the bubble does not
        // stretch to the cap for short text.
        assert_eq!(end - start + 1, 6, "{:?}", rows[top]);
        assert!(rows[top + 1].contains("│ hi │"), "{:?}", rows[top + 1]);
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
        settings.sections().len() + 3,
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
    assert!(text.contains("no servers in this sandbox"), "{text}");
    assert!(
        text.contains("the add row below installs one"),
        "the empty state nudges at the add row: {text}"
    );
}
/// A session notification renders as its own card: the shared bubble
/// frame, hugging the left edge, neither the prompt's right rail nor
/// the response's bullet, and named by the neutral fill, not the
/// accent.
#[test]
fn a_notification_renders_as_a_left_aligned_card() {
    let theme = DARK;
    let lines = vec![crate::transcript::Line::Notification(
        "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned(),
    )];
    let blocks = pane::partition(&lines);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, pane::Kind::Notification);
    // Wide enough that the sentence fits one padded row whole.
    let (rows, bubbles) = pane::render_block(&lines, &blocks[0], &theme, 200, "r");
    let geometry = bubbles[0];
    assert_eq!(geometry.left, 0, "the card hugs the left edge");
    assert_eq!(geometry.fill, pane::CardFill::Notice);
    assert!(rows[0].spans.iter().any(|s| s.content.contains('╭')));
    let flat: String = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert!(
        flat.contains("has finished (exit 0)"),
        "the card carries the line: {flat}"
    );
    // Narrow, the card still frames: the sentence wraps, the glyphs hold.
    let (narrow, _) = pane::render_block(&lines, &blocks[0], &theme, 40, "r");
    assert!(narrow[0].spans.iter().any(|s| s.content.contains('╭')));
    assert!(narrow.len() > 2, "the body wrapped: {}", narrow.len());
}

/// A run of finishes coalesces into one block, one stacked card per
/// finish, the tick's joined prompt tells the model once, the
/// transcript shows the same count.
#[test]
fn a_run_of_notifications_is_one_block_of_stacked_cards() {
    let theme = DARK;
    let lines = vec![
        crate::transcript::Line::Notification(
            "Background job abc (\"a\") has finished (exit 0).".to_owned(),
        ),
        crate::transcript::Line::Notification(
            "Background job def (\"b\") has finished with an error (exit 1).".to_owned(),
        ),
    ];
    let blocks = pane::partition(&lines);
    assert_eq!(blocks.len(), 1, "both cards sit in one block");
    assert_eq!(blocks[0].start, 0);
    assert_eq!(blocks[0].end, 2);
    let (_, bubbles) = pane::render_block(&lines, &blocks[0], &theme, 80, "r");
    assert_eq!(bubbles.len(), 2, "one card per finish");
    assert!(bubbles.iter().all(|geometry| geometry.left == 0));
}

/// The web search page draws one line per provider and exactly one
/// add line: the sentinel closing the rows vec must not render as
/// its own `·  ·` ghost between the two.
#[test]
fn the_websearch_page_renders_no_ghost_add_row() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::WebSearch;
    settings.websearch = crate::screens::websearch_rows(
        &std::collections::BTreeMap::from([
            (
                "Exa".to_owned(),
                rhema_config::WebSearchInstance {
                    provider: "exa".to_owned(),
                    api_key: None,
                    secret_ref: None,
                },
            ),
            (
                "Tavily".to_owned(),
                rhema_config::WebSearchInstance {
                    provider: "tavily".to_owned(),
                    api_key: None,
                    secret_ref: None,
                },
            ),
        ]),
        &["Exa".to_owned(), "Tavily".to_owned()],
    );

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
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("1. Exa · exa"),
        "the provider rows render\n{text}"
    );
    assert!(text.contains("2. Tavily · tavily"));
    assert_eq!(
        text.matches("Add a provider").count(),
        1,
        "one add line, the loop's ghost gone\n{text}"
    );
    assert!(
        !text.contains("·  ·"),
        "no sentinel line between the providers and the add row\n{text}"
    );
    // The add line is the only PageRow hit at the row after the
    // providers: no doubled click target.
    let add_hits = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, SettingsHit::PageRow(3)))
        .count();
    assert_eq!(add_hits, 1, "one click target on the add row\n{text}");
}

/// The Decision models page: one row per instance with its kind, its
/// model (the kind's default for a blank), its endpoint when it has
/// one, the default mark, and the add row once, closing the list.
#[test]
fn the_decision_page_renders_its_rows_and_one_add_row() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Decision;
    let mut section = rhema_config::DecisionSettings {
        instance: Some("Local Jev".into()),
        ..Default::default()
    };
    section.instances.insert(
        "Jev".into(),
        rhema_config::DecisionInstance {
            provider: "jev".into(),
            api_key: Some("k".into()),
            secret_ref: None,
            model: Some("jev-preview".into()),
            account_id: None,
            base_url: None,
        },
    );
    section.instances.insert(
        "Local Jev".into(),
        rhema_config::DecisionInstance {
            provider: "jev".into(),
            api_key: None,
            secret_ref: None,
            model: None,
            account_id: None,
            base_url: Some("http://localhost:30000".into()),
        },
    );
    settings.decision = crate::screens::decision_rows(Some(&section), None);
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text: String = (0..40)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Decision models"),
        "the sidebar and the title: {text}"
    );
    assert!(
        text.contains("Jev · jev · jev-preview"),
        "the instance's own model: {text}"
    );
    assert!(
        // The kind reads as stored, the file's spelling.
        text.contains("Local Jev · jev · jev-latest · http://localhost:30000 · no key · default"),
        "the default, the blank model's default, the endpoint, the missing key: {text}"
    );
    assert_eq!(
        text.matches("Add a decision model").count(),
        1,
        "one add row: {text}"
    );
    assert!(
        text.contains("enter toggles the switch, sets the default"),
        "the footer hints: {text}"
    );
    // The switch, two instance rows, and the add row register their
    // click targets.
    let page_rows = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, SettingsHit::PageRow(_)))
        .count();
    assert_eq!(page_rows, 4, "{hits:?}");
}

/// The empty page names the fact; the add row below it is the way
/// in, stated by the row itself, not by a second hint line.
#[test]
fn the_decision_page_without_instances_states_the_fact() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Decision;
    settings.decision = crate::screens::decision_rows(None, None);
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let text: String = (0..20)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(
        text.contains("no decision models in this profile"),
        "{text}"
    );
    assert!(!text.contains("TYPESAFE"), "{text}");
}

/// The removal confirm over the Decision models page names the
/// instance and its answer.
#[test]
fn the_decision_confirm_names_the_instance_and_its_answer() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Decision;
    settings.decision_confirm = Some("Jev".to_owned());
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
    assert!(text.contains(" Confirmation "), "{text}");
    assert!(
        text.contains("Are you sure you want to delete provider Jev?"),
        "{text}"
    );
    assert!(text.contains("y/n"), "{text}");
}

/// The add dialog: the four kinds on the kind stage, the input line
/// on the others, a key masked to its length, the hint or the error on
/// the status line.
#[test]
fn the_add_decision_dialog_renders_its_kinds_its_stages_and_the_masked_key() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.section = SettingsSection::Decision;
    settings.decision_dialog = Some(crate::screens::AddDecisionDialog::new(BTreeSet::new()));
    let draw = |settings: &crate::screens::SettingsScreen| {
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = draw(&settings);
    assert!(text.contains("Add a decision model"), "{text}");
    for display in ["Jev", "Clef", "OpenRouter", "Custom Jev endpoint"] {
        assert!(text.contains(display), "{display} on the kind list: {text}");
    }
    assert!(text.contains("❯ Jev"), "the first row highlighted: {text}");
    let press = |settings: &mut crate::screens::SettingsScreen, code: KeyCode| {
        settings
            .decision_dialog
            .as_mut()
            .unwrap()
            .handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    };
    // Down to Clef, enter: the name line carries the suggestion and
    // its label; enter again reaches the account line.
    press(&mut settings, KeyCode::Down);
    press(&mut settings, KeyCode::Enter);
    let text = draw(&settings);
    assert!(text.contains("name: Clef"), "{text}");
    assert!(text.contains("name this instance"), "{text}");
    press(&mut settings, KeyCode::Enter);
    let text = draw(&settings);
    assert!(text.contains("account id: "), "{text}");
    assert!(text.contains("Cloudflare account id"), "{text}");
    // A blank enter leaves the error on the status line, in the warn
    // color.
    press(&mut settings, KeyCode::Enter);
    let text = draw(&settings);
    assert!(text.contains("An account id is required"), "{text}");
    // To the key line: the text shows its length, never its bytes.
    for c in "acct".chars() {
        press(&mut settings, KeyCode::Char(c));
    }
    press(&mut settings, KeyCode::Enter);
    for c in "sk-secret-1".chars() {
        press(&mut settings, KeyCode::Char(c));
    }
    let text = draw(&settings);
    assert!(text.contains("api key: "), "{text}");
    assert!(text.contains("(11 chars)"), "{text}");
    assert!(
        !text.contains("sk-secret-1"),
        "the key never renders: {text}"
    );
}

/// A page with no providers says so above its add row, on both pages
/// built from the web search template: the rows always carry the add
/// row, so "empty" is no instance above it.
#[test]
fn the_websearch_and_decision_pages_say_so_when_empty() {
    let draw = |section: SettingsSection| {
        let mut settings = crate::screens::SettingsScreen::new();
        settings.section = section;
        settings.websearch = crate::screens::websearch_rows(&BTreeMap::new(), &[]);
        settings.decision = crate::screens::decision_rows(None, None);
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        (0..16)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let web = draw(SettingsSection::WebSearch);
    assert!(web.contains("no providers in this profile"), "{web}");
    assert!(web.contains("+ Add a provider..."), "{web}");
    let decision = draw(SettingsSection::Decision);
    assert!(
        decision.contains("no decision models in this profile"),
        "{decision}"
    );
    assert!(decision.contains("+ Add a decision model..."), "{decision}");
}

/// The picker adapts to the width it is given: a narrow pane keeps the
/// names and drops the price, capabilities, and context tail, which is
/// what a host embedding it in a sidebar relies on.
#[test]
fn the_picker_drops_the_details_when_the_pane_is_narrow() {
    let mut card = model("model-a");
    card.input_token_limit = Some(200_000);
    card.pricing = Some(rhema_provider::Pricing {
        input_per_mtok: 1.25,
        output_per_mtok: 10.0,
        cached_input_per_mtok: None,
    });
    let picker = PickerScreen::new(vec![CatalogModel {
        provider: "google",
        instance: None,
        model: card,
    }]);
    let rows = |width: u16| {
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal
            .draw(|frame| render_picker(frame, &picker, &crate::icons::UNICODE, &DARK))
            .unwrap();
        (0..8)
            .map(|y| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let wide = rows(80).join("\n");
    // The numbers sit in titled columns, not in a run-on tail.
    assert!(wide.contains("Model"), "{wide}");
    assert!(wide.contains("google/model-a  $1.25"), "{wide}");
    assert!(wide.contains("Out"), "{wide}");
    let narrow = rows(30).join("\n");
    assert!(narrow.contains("google/model-a"), "{narrow}");
    assert!(!narrow.contains("Out"), "the widest column goes: {narrow}");
    let tiny = rows(20).join("\n");
    assert!(tiny.contains("google/model-a"), "{tiny}");
    assert!(!tiny.contains('$'), "the numbers go last: {tiny}");
}

/// The component draws into whatever rect a host hands it, down to a
/// single row; the footer only appears when the area can spare it.
#[test]
fn the_picker_fills_a_short_inline_area() {
    let picker = ModelPicker::new(vec![CatalogModel {
        provider: "google",
        instance: None,
        model: model("model-a"),
    }]);
    let draw = |height: u16| {
        let area = Rect::new(2, 1, 30, height);
        let mut terminal = Terminal::new(TestBackend::new(36, 8)).unwrap();
        terminal
            .draw(|frame| {
                super::model_picker::render_model_picker(
                    frame,
                    area,
                    &picker,
                    &crate::icons::UNICODE,
                    &DARK,
                    &super::model_picker::PickerChrome {
                        title: Some("Models"),
                        hints: "",
                        focused: false,
                        prompt: "",
                    },
                );
            })
            .unwrap();
        (0..8)
            .map(|y| {
                (0..36)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    // Four rows: the title, the one model, and the status line.
    let tall = draw(4).join("\n");
    assert!(tall.contains("Models"), "{tall}");
    assert!(tall.contains("❯ google/model-a"), "{tall}");
    assert!(tall.contains("1 models"), "{tall}");
    // One row: the model alone, no title and no footer.
    let flat = draw(1).join("\n");
    assert!(flat.contains("google/model-a"), "{flat}");
    assert!(!flat.contains("Models"), "{flat}");
    assert!(!flat.contains("1 models"), "{flat}");
}

/// The pinned current model reads as the profile's own, not as a catalog
/// row: no price, no context, and an `in use` tag.
#[test]
fn the_current_row_reads_as_in_use() {
    let picker = ModelPicker::new(Vec::new()).with_current(Some(CurrentModel {
        provider: "google".into(),
        instance: None,
        id: "gemini-x".into(),
    }));
    let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
    terminal
        .draw(|frame| {
            super::model_picker::render_model_picker(
                frame,
                Rect::new(0, 0, 60, 6),
                &picker,
                &crate::icons::UNICODE,
                &DARK,
                &super::model_picker::PickerChrome {
                    title: None,
                    hints: "",
                    focused: true,
                    prompt: "",
                },
            );
        })
        .unwrap();
    let mut text = String::new();
    for y in 0..6 {
        for x in 0..60 {
            text.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
        text.push(' ');
    }
    assert!(text.contains("❯ google/gemini-x"), "{text}");
    assert!(text.contains("· in use"), "{text}");
}

/// A terminal too short for the whole page scrolls it to the selected
/// row instead of clipping it, and the rows the scroll pushed out of
/// the pane take no clicks.
#[test]
fn a_short_providers_page_scrolls_to_its_selected_row() {
    let mut settings = crate::screens::SettingsScreen::new();
    for name in ["a", "b"] {
        settings.stored.instances.insert(
            name.to_owned(),
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some(format!("http://{name}:8080")),
                api: None,
            },
        );
    }
    settings.stored.instance = Some("a".into());
    // The add row is the last one; selecting it scrolls the pane.
    settings.row = 2;

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let mut screen = String::new();
    for y in 0..16 {
        for x in 0..60 {
            screen.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
    }
    assert!(screen.contains("+ Add a new instance..."), "{screen}");
    let page_rows = hits
        .iter()
        .filter(|(_, hit)| matches!(hit, crate::app::SettingsHit::PageRow(_)))
        .count();
    assert_eq!(page_rows, 3, "two instances plus the add row: {screen}");
}

/// The Models page while the catalog is out says so: a spinner on the
/// list's text column, no search box to type into, and the one key a
/// keyless page answers to.
#[test]
fn the_models_page_spins_while_the_catalog_loads() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    settings.model_picker.start_loading();
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let mut screen = String::new();
    for y in 0..14 {
        for x in 0..60 {
            screen.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
    }
    assert!(screen.contains("loading models…"), "{screen}");
    let spinner = screen
        .chars()
        .find(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(*c))
        .expect("the loading line leads with a spinner");
    assert_eq!(
        spinner,
        crate::ui::status::spinner_char(settings.model_picker.loading_for()),
        "the frame steps off the picker's clock"
    );
    assert!(
        !screen.contains("filter:"),
        "no search box before a catalog: {screen}"
    );
    assert_eq!(
        settings.enter_action(),
        crate::screens::SettingsAction::ChooseModel,
        "the page still routes Enter, its picker has nothing to give"
    );
    assert!(
        screen.contains("esc back"),
        "the one key that answers: {screen}"
    );
}

/// While a batch is still out over rows that landed, the page draws
/// those rows, spins where the filter box will be, names the provider
/// that died, and offers the keys that still answer.
#[test]
fn the_models_page_draws_the_rows_that_landed_while_more_are_out() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    settings.model_picker.set_models(vec![CatalogModel {
        provider: "google",
        instance: None,
        model: model("model-a"),
    }]);
    settings.stored.provider = Some("google".into());
    settings.stored.model = Some("model-a".into());
    settings.sync_current_model();
    settings.model_picker.start_loading();
    settings.model_picker.failed = vec!["llama-cpp"];
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let mut screen = String::new();
    for y in 0..14 {
        for x in 0..80 {
            screen.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
    }
    assert!(
        screen.contains("google/model-a"),
        "the landed row: {screen}"
    );
    assert!(screen.contains("loading…"), "the spinner: {screen}");
    assert!(
        screen.contains("no models from llama-cpp"),
        "the dead provider: {screen}"
    );
    assert!(!screen.contains("filter:"), "no filter box yet: {screen}");
    assert!(
        screen.contains("enter choose · ctrl-a all · esc back"),
        "the keys that answer: {screen}"
    );
    let rows: Vec<usize> = hits
        .iter()
        .filter_map(|(_, hit)| match hit {
            crate::app::SettingsHit::PageRow(index) => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(rows, [0], "the landed row is clickable");
}

/// The Models page is the catalog as a page: one cursor, the page's,
/// walking the list, a filter line always drawn, the model in use
/// marked, and every drawn row clickable.
#[test]
fn the_models_page_lists_the_catalog_under_the_page_cursor() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    settings.model_picker.set_models(vec![
        CatalogModel {
            provider: "google",
            instance: None,
            model: model("model-a"),
        },
        CatalogModel {
            provider: "google",
            instance: None,
            model: model("model-b"),
        },
    ]);
    settings.stored.provider = Some("google".into());
    settings.stored.model = Some("model-a".into());
    settings.sync_current_model();
    assert_eq!(
        settings.model_picker.selected, 0,
        "the cursor opens on the model in use"
    );
    settings.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(
        settings.model_picker.selected, 1,
        "the page cursor moves it"
    );
    assert_eq!(
        settings.enter_action(),
        crate::screens::SettingsAction::ChooseModel
    );

    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let mut screen = String::new();
    for y in 0..14 {
        for x in 0..60 {
            screen.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
    }
    assert!(screen.contains("Models"), "{screen}");
    assert!(screen.contains("google/model-a"), "{screen}");
    assert!(
        screen.contains("❯ google/model-b"),
        "the page cursor: {screen}"
    );
    assert!(screen.contains("· in use"), "the model that runs: {screen}");
    assert!(screen.contains("filter:"), "the search box: {screen}");
    assert!(
        screen.contains("enter choose"),
        "the page's hints: {screen}"
    );
    // The page's cursor sits in the page's own text column, under the
    // title, where every other page's row marker sits; the id follows a
    // cell later.
    // The page's cursor sits in the page's own text column, under the
    // title, where every other page's row marker sits; the id follows a
    // cell later.
    let row_cells = |y: u16| -> Vec<String> {
        (0..60)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
            .collect()
    };
    // Cells are one symbol each: a byte offset would miss on the
    // multi-byte glyphs the sidebar draws.
    let column_of = |y: u16, needle: &str| -> Option<usize> {
        let cells = row_cells(y);
        (0..cells.len()).find(|start| cells[*start..].concat().starts_with(needle))
    };
    let selected_row = (0..14)
        .find(|y| row_cells(*y).concat().contains("google/model-b"))
        .expect("the selected row is drawn");
    let id_x = column_of(selected_row, "google/model-b").expect("the id is drawn");
    assert_eq!(
        row_cells(selected_row)[id_x - 2],
        "❯",
        "the marker sits before the id: {screen}"
    );
    let heading = (0..14)
        .find(|y| row_cells(*y).concat().contains('⚙'))
        .expect("the heading row");
    assert_eq!(
        column_of(heading, "Models"),
        Some(id_x - 2),
        "the list sits in the page's text column: {screen}"
    );
    let rows: Vec<usize> = hits
        .iter()
        .filter_map(|(_, hit)| match hit {
            crate::app::SettingsHit::PageRow(index) => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(rows, [0, 1], "each drawn row is clickable");
}

/// The catalog is a table: every column is titled, each title sits over
/// its own cells (`Model` over the names it titles), and the in-use
/// row's mark rides inside the model column so the numbers below it
/// still line up.
#[test]
fn the_models_page_lays_the_catalog_out_in_titled_columns() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    let card = |id: &str, in_price: f64, out_price: f64, vision: bool, ctx: u64| {
        let mut model = model(id);
        model.pricing = Some(rhema_provider::Pricing {
            input_per_mtok: in_price,
            cached_input_per_mtok: None,
            output_per_mtok: out_price,
        });
        model.supports_vision = Some(vision);
        model.input_token_limit = Some(ctx);
        model
    };
    settings.model_picker.set_models(vec![
        crate::screens::CatalogModel {
            provider: "mistral-vibe",
            instance: None,
            model: card("mistral-large-4-0", 0.70, 2.10, true, 1_048_576),
        },
        crate::screens::CatalogModel {
            provider: "opencode",
            instance: None,
            model: card("glm-5.3", 1.40, 4.40, true, 1_048_576),
        },
    ]);
    settings.stored.provider = Some("mistral-vibe".into());
    settings.stored.model = Some("mistral-large-4-0".into());
    settings.sync_current_model();
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let row = |y: u16| -> Vec<String> {
        (0..120)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
            .collect()
    };
    let column_of = |y: u16, needle: &str| -> Option<usize> {
        let cells = row(y);
        (0..cells.len()).find(|start| cells[*start..].concat().starts_with(needle))
    };
    let header = (0..26)
        .find(|y| row(*y).concat().contains("Ctx"))
        .expect("the header row");
    let in_use_row = (0..26)
        .find(|y| row(*y).concat().contains("mistral-vibe/mistral-large-4-0"))
        .expect("the in-use row");
    let other = (0..26)
        .find(|y| row(*y).concat().contains("opencode/glm-5.3"))
        .expect("the other row");
    for (title, cells) in [
        ("In", ["$0.70", "$1.40"]),
        ("Out", ["$2.10", "$4.40"]),
        ("Cap.", ["◉", "◉"]),
        ("Ctx", ["1.0M", "1.0M"]),
    ] {
        let title_x = column_of(header, title).unwrap_or_else(|| panic!("{title} is titled"));
        for (y, cell) in [(in_use_row, cells[0]), (other, cells[1])] {
            assert_eq!(
                column_of(y, cell),
                Some(title_x),
                "{title} sits over its cells"
            );
        }
    }
    // `Model` names the names, so it starts over them.
    assert_eq!(
        column_of(header, "Model"),
        column_of(in_use_row, "mistral-vibe"),
        "the model column's title"
    );
    // The mark rides in the model column, never in a number's.
    assert!(
        row(in_use_row)
            .concat()
            .contains("mistral-large-4-0  · in use"),
        "the in-use mark: {}",
        row(in_use_row).concat()
    );
}

/// A narrow pane drops columns before it squeezes the names: an id
/// differentiates models at its tail, so it keeps the room.
#[test]
fn a_narrow_pane_drops_the_columns_before_the_names() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    let mut card = model("zai-glm-5-3-flash-vibe-free");
    card.input_token_limit = Some(262_144);
    card.pricing = Some(rhema_provider::Pricing {
        input_per_mtok: 0.15,
        cached_input_per_mtok: None,
        output_per_mtok: 0.50,
    });
    settings
        .model_picker
        .set_models(vec![crate::screens::CatalogModel {
            provider: "mistral-vibe",
            instance: None,
            model: card,
        }]);
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(64, 18)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let mut text = String::new();
    for y in 0..18 {
        for x in 0..64 {
            text.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
    }
    assert!(text.contains("Model"), "the names keep their title: {text}");
    assert!(
        !text.contains("$") && !text.contains("Ctx"),
        "the numbers go: {text}"
    );
}

/// The page keeps its air: two rows under the border for a tall
/// terminal, three cells inside the sidebar, and the hints a row above
/// the bottom border — with every section still on a short one.
#[test]
fn the_settings_page_keeps_its_air() {
    let draw = |height: u16| -> Vec<String> {
        let settings = crate::screens::SettingsScreen::new();
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(90, height)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        (0..height)
            .map(|y| {
                (0..90)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    };
    let tall = draw(30);
    let title = tall
        .iter()
        .position(|row| row.contains('⚙'))
        .expect("the heading row");
    assert_eq!(title, 3, "two rows of air under the border");
    let inside = |row: &String| {
        // The border's own cells are not the page's.
        row.chars()
            .skip(1)
            .take(88)
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let last = (1..29)
        .rev()
        .find(|y| !inside(&tall[*y as usize]).is_empty())
        .expect("the page draws something");
    assert_eq!(last, 30 - 3, "the hints sit a row above the border");
    // A short terminal gives the top row back: the sections all fit.
    let short = draw(18);
    for section in crate::screens::SettingsScreen::new().sections() {
        assert!(
            short.iter().any(|row| row.contains(section.label())),
            "{} on a short page",
            section.label()
        );
    }
}

/// The titles read as titles: bold, a tone above the cells they name,
/// with a row of air under them.
#[test]
fn the_catalog_titles_are_typeset_apart_from_their_cells() {
    let mut settings = crate::screens::SettingsScreen::new();
    settings.focus_section(SettingsSection::Models);
    let mut card = model("glm-5.3");
    card.input_token_limit = Some(1_048_576);
    card.supports_vision = Some(true);
    card.pricing = Some(rhema_provider::Pricing {
        input_per_mtok: 1.40,
        cached_input_per_mtok: None,
        output_per_mtok: 4.40,
    });
    settings
        .model_picker
        .set_models(vec![crate::screens::CatalogModel {
            provider: "opencode",
            instance: None,
            model: card,
        }]);
    let mut hits = Vec::new();
    let mut terminal = Terminal::new(TestBackend::new(110, 26)).unwrap();
    terminal
        .draw(|frame| render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits))
        .unwrap();
    let row = |y: usize| -> Vec<(String, ratatui::style::Style)> {
        (0..110)
            .map(|x| {
                let cell = &terminal.backend().buffer()[(x as u16, y as u16)];
                (cell.symbol().to_owned(), cell.style())
            })
            .collect()
    };
    let text = |y: usize| row(y).iter().map(|(s, _)| s.clone()).collect::<String>();
    let header = (0..26)
        .find(|y| text(*y).contains("Cap."))
        .expect("the header");
    let line = (0..26)
        .find(|y| text(*y).contains("opencode/glm-5.3"))
        .expect("the row");
    assert!(line > header, "titles come first");
    assert_eq!(line, header + 2, "a row of air under the titles");
    // Past the sidebar: its labels are the screen's, not the table's.
    for x in 24..110 {
        let (symbol, style) = &row(header)[x];
        if symbol.trim().is_empty() || "│╭╮╰╯─".contains(symbol.as_str()) {
            continue;
        }
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "the title cells are bold: {symbol:?}"
        );
    }
    // The cells are not bold: the title has to stand apart from them.
    let price = (0..110)
        .find(|x| row(line)[*x].0 == "$")
        .expect("the price cell");
    assert!(!row(line)[price].1.add_modifier.contains(Modifier::BOLD));
    assert_ne!(
        row(line)[price].1.fg,
        row(header)[price].1.fg,
        "the title sits a tone apart from the cell under it"
    );
}

/// The page hands the picker its whole body: a catalog that shrank the
/// block as rows were filtered away would walk the filter box up out
/// from under the hands typing in it.
#[test]
fn the_models_page_keeps_its_filter_box_where_it_stands() {
    let draw = |count: usize| -> (usize, usize, usize) {
        let mut settings = crate::screens::SettingsScreen::new();
        settings.focus_section(SettingsSection::Models);
        let cards = (0..count)
            .map(|n| {
                let mut card = model(&format!("model-{n}"));
                card.input_token_limit = Some(1_048_576);
                card.pricing = Some(rhema_provider::Pricing {
                    input_per_mtok: 0.70,
                    cached_input_per_mtok: None,
                    output_per_mtok: 2.10,
                });
                crate::screens::CatalogModel {
                    provider: "mistral-vibe",
                    instance: None,
                    model: card,
                }
            })
            .collect();
        settings.model_picker.set_models(cards);
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        let text = |y: usize| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x as u16, y as u16)].symbol())
                .collect::<String>()
        };
        let header = (0..26)
            .find(|y| text(*y).contains("Model"))
            .expect("the header");
        let filter = (0..26)
            .find(|y| text(*y).contains("filter:"))
            .expect("the filter line");
        (header, header + 2, filter)
    };
    let (header, first, filter) = draw(3);
    assert_eq!(first, header + 2, "one row of air under the titles");
    let (small_header, _, small_filter) = draw(1);
    let (many_header, _, many_filter) = draw(12);
    assert_eq!(small_header, header);
    assert_eq!(many_header, header);
    assert_eq!(
        small_filter, many_filter,
        "the filter box holds its row whatever the catalog holds"
    );
    assert!(filter > first, "the rows sit above the filter");
}

/// A filter that matches nothing keeps its titles and says so in the
/// column the rows would have filled: the table does not collapse into
/// a bare "no match", and the columns it names are the catalog's, not
/// whatever the filter left.
#[test]
fn a_filter_with_no_match_keeps_the_titles_over_its_message() {
    let draw = |filter: &str| -> (Vec<(usize, String)>, usize, String) {
        let mut settings = crate::screens::SettingsScreen::new();
        settings.focus_section(SettingsSection::Models);
        let cards = ["aaaa-long-model-id", "b", "c-vision"]
            .into_iter()
            .map(|id| {
                let mut card = model(id);
                card.supports_vision = (id == "c-vision").then_some(true);
                card.input_token_limit = Some(1_048_576);
                card.pricing = Some(rhema_provider::Pricing {
                    input_per_mtok: 0.70,
                    cached_input_per_mtok: None,
                    output_per_mtok: 2.10,
                });
                crate::screens::CatalogModel {
                    provider: "mistral-vibe",
                    instance: None,
                    model: card,
                }
            })
            .collect();
        settings.model_picker.set_models(cards);
        settings.model_picker.filter = filter.to_owned();
        let mut hits = Vec::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        terminal
            .draw(|frame| {
                render_settings(frame, &settings, &DARK, &crate::icons::UNICODE, &mut hits)
            })
            .unwrap();
        let text = |y: usize| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x as u16, y as u16)].symbol())
                .collect::<String>()
        };
        let header = (0..18)
            .find(|y| text(*y).contains("Cap."))
            .expect("the header");
        let titles = ["Model", "In", "Out", "Cap.", "Ctx"]
            .into_iter()
            .map(|title| (text(header).find(title).expect(title), title.to_owned()))
            .collect();
        (titles, header, text(header + 2))
    };
    let (full_titles, full_header, _) = draw("");
    let (titles, header, message) = draw("zzzz");
    assert_eq!(titles, full_titles, "the columns hold their places");
    assert_eq!(header, full_header, "the titles hold their row");
    assert!(message.contains("no match"), "{message}");
    assert!(
        !message.contains("Model"),
        "the titles are a row of their own: {message}"
    );
    // Under the data, not under the marker column.
    let indent = message.find("no match").expect("the message");
    assert_eq!(
        indent, full_titles[0].0,
        "the message starts where a row does"
    );
}

/// A resume row is two lines: the prompt the pane can hold, then its
/// age and size. A summary wider than the pane is cut with an ellipsis
/// rather than clipped mid-word at the panel's border.
#[test]
fn the_resume_picker_draws_two_lines_a_session() {
    let now = 1_000_000_000_000u64;
    let screen = crate::screens::ResumeScreen::new(
        vec![
            crate::sessions::SavedSession {
                path: "first.json".into(),
                saved_at: now - 60_000,
                snippet: Some(
                    "a prompt that runs long enough to need the row's ellipsis, twice over"
                        .to_owned(),
                ),
                bytes: 66_403,
                workspace: "/home/u/projects/rhema".into(),
            },
            crate::sessions::SavedSession {
                path: "second.json".into(),
                saved_at: now - 7_200_000,
                snippet: None,
                bytes: 512,
                workspace: "/home/u/projects/rhema".into(),
            },
        ],
        now,
        false,
    );
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
    terminal
        .draw(|frame| super::dialogs::render_resume_picker(frame, &screen, &DARK, None))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: Vec<String> = (0..10)
        .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();

    assert!(
        text[1]
            .trim_matches(|ch: char| ch == '│' || ch.is_whitespace())
            .is_empty(),
        "a row of air under the frame: {text:#?}"
    );
    assert!(
        text[2].contains('❯'),
        "the marker rides the summary: {text:#?}"
    );
    assert!(
        text[2].contains('…'),
        "the summary is ellipsized to the pane: {text:#?}"
    );
    assert!(
        !text[2].contains("twice over"),
        "the tail does not fit: {text:#?}"
    );
    let inner = |y: usize| {
        text[y]
            .trim_matches(|ch: char| ch == '│' || ch.is_whitespace())
            .to_owned()
    };
    assert_eq!(inner(3), "1m ago · 65 KB", "{text:#?}");
    assert_eq!(
        text[2].chars().position(|ch| ch == 'a'),
        text[3].chars().position(|ch| ch == '1'),
        "the summary and its metadata share a left margin: {text:#?}"
    );
    assert!(text[4].contains("(no preview)"), "{text:#?}");
    assert_eq!(inner(5), "2h ago · 512 B", "{text:#?}");
}

/// Across every workspace, the metadata line names the workspace
/// before the age and size, trimmed the footer's way.
#[test]
fn the_all_workspaces_resume_picker_names_each_workspace() {
    let now = 1_000_000_000_000u64;
    let mut screen = crate::screens::ResumeScreen::new(
        vec![crate::sessions::SavedSession {
            path: "first.json".into(),
            saved_at: now - 60_000,
            snippet: Some("a prompt".to_owned()),
            bytes: 66_403,
            workspace: "/home/u/projects/rhema".into(),
        }],
        now,
        true,
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
    terminal
        .draw(|frame| {
            super::dialogs::render_resume_picker(
                frame,
                &screen,
                &DARK,
                Some(std::path::Path::new("/home/u")),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: Vec<String> = (0..8)
        .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    assert!(
        text[3].contains("projects/rhema · 1m ago · 65 KB"),
        "the workspace precedes the facts: {text:#?}"
    );
    assert!(
        text.iter().any(|row| row.contains("ctrl-a this workspace")),
        "the footer names the way back: {text:#?}"
    );

    // The workspace gives way first when the pane is narrow.
    screen.show_all = true;
    let mut terminal = Terminal::new(TestBackend::new(28, 8)).unwrap();
    terminal
        .draw(|frame| {
            super::dialogs::render_resume_picker(
                frame,
                &screen,
                &DARK,
                Some(std::path::Path::new("/home/u")),
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let row: String = (0..28).map(|x| buffer[(x, 3)].symbol()).collect();
    assert!(row.contains('…'), "the workspace is the part cut: {row:?}");
    assert!(row.contains("1m ago"), "the age survives: {row:?}");
    assert!(row.contains("65 KB"), "the size survives: {row:?}");
}
