//! The screens' tests: the state machines run headless against typed
//! inputs and assert on the outcomes.

use super::palette::{model_description, path_token};
use super::*;

use crate::args::SandboxChoice;
use crate::sandbox::SandboxAvailability;
use rhema_provider::ModelCard;

/// What this machine offers, spelled per test.
#[allow(dead_code)]
fn offered(docker: bool, ssh: bool, http: bool) -> SandboxAvailability {
    SandboxAvailability {
        docker,
        lxd: false,
        ecrou: false,
        ssh,
        http,
    }
}

/// A settings screen on the Sandbox page, with the snapshot filled.
fn sandbox_screen(
    availability: SandboxAvailability,
    remembered: Option<SandboxChoice>,
    active: SandboxChoice,
    stored: rhema_config::Profile,
) -> SettingsScreen {
    let mut settings = SettingsScreen::new();
    settings.section = SettingsSection::Sandbox;
    settings.sandbox_offered = availability;
    settings.sandbox_remembered = remembered;
    settings.sandbox_active = active;
    settings.stored = stored;
    settings
}

/// A stored instance under `name`.
fn instance(kind: &str, target: &str) -> rhema_config::SandboxInstance {
    rhema_config::SandboxInstance {
        kind: kind.to_owned(),
        target: Some(target.to_owned()),
        vm: None,
    }
}

#[test]
fn the_sandbox_page_lists_local_then_the_instances_then_add() {
    // An empty config: the host's own row and the add row — the
    // default view is only local, the ready kinds arriving as
    // instances are stored.
    let settings = sandbox_screen(
        offered(false, true, false),
        None,
        SandboxChoice::Local,
        rhema_config::Profile::default(),
    );
    let rows = settings.sandbox_rows();
    assert_eq!(
        rows.iter().map(|row| row.name).collect::<Vec<_>>(),
        vec!["local", ""],
        "only local until an instance is stored; the add row last"
    );
    // Stored instances list in name order, whatever their kind —
    // the page shows what is configured; the add dialog decides
    // what can be added.
    let mut stored = rhema_config::Profile::default();
    stored.place_sandbox_instance("build", instance("lxd", "build-vm"));
    stored.place_sandbox_instance("dev", instance("docker", "rhema-dev"));
    stored.sandbox = Some(rhema_config::SandboxSettings {
        active: Some("build".to_owned()),
        ..rhema_config::SandboxSettings::default()
    });
    let settings = sandbox_screen(
        offered(false, true, false),
        None,
        SandboxChoice::Local,
        stored,
    );
    assert_eq!(
        settings
            .sandbox_rows()
            .iter()
            .map(|row| row.name)
            .collect::<Vec<_>>(),
        vec!["local", "build", "dev", ""],
        "the map's name order; the add row sits last"
    );
    // The rows plus the add row are the arrows' range: four downs
    // from the top wrap back to it.
    let mut stepped = sandbox_screen(
        offered(false, true, false),
        None,
        SandboxChoice::Local,
        rhema_config::Profile::default(),
    );
    for _ in 0..2 {
        stepped.handle_key(key(KeyCode::Down));
    }
    assert_eq!(stepped.row, 0, "the range wraps at two rows");
}

#[test]
fn the_sandbox_rows_carry_the_instance_facts_and_the_marks() {
    let mut stored = rhema_config::Profile::default();
    stored.place_sandbox_instance("dev", instance("docker", "rhema-dev"));
    stored.place_sandbox_instance("box", instance("ssh", "deploy@far"));
    stored.place_sandbox_instance(
        "dev-vm",
        rhema_config::SandboxInstance {
            kind: "lxd".to_owned(),
            target: Some("dev".to_owned()),
            vm: Some(true),
        },
    );
    stored.sandbox = Some(rhema_config::SandboxSettings {
        kind: Some("docker".to_owned()),
        active: Some("dev".to_owned()),
        ..rhema_config::SandboxSettings::default()
    });
    let settings = sandbox_screen(
        offered(true, false, false),
        Some(SandboxChoice::Docker),
        SandboxChoice::Docker,
        stored,
    );
    let rows = settings.sandbox_rows();
    let by_name = |name: &str| {
        rows.iter()
            .find(|row| row.name == name && !row.add)
            .expect("row present")
    };
    // The kind and the target read off the instance; a vm shows in
    // the detail.
    assert_eq!(by_name("dev").kind, SandboxChoice::Docker);
    assert_eq!(by_name("dev").detail, "rhema-dev");
    assert_eq!(by_name("dev-vm").detail, "dev · vm");
    // The marks: offered by the kind's probe, remembered by the
    // workspace memory's kind, active by the section's pointer.
    assert!(by_name("dev").offered);
    assert!(by_name("dev").remembered);
    assert!(by_name("dev").active);
    assert!(!by_name("dev-vm").active, "the pointer names dev");
    // The lxd probe answered no: the instance lists anyway, the
    // mark says why it cannot run.
    assert!(!by_name("dev-vm").offered);
    // ssh is real config: it lists, and its offer needs the
    // binary plus the probe.
    assert_eq!(by_name("box").kind, SandboxChoice::Ssh);
    assert!(!by_name("box").offered);
}

#[test]
fn sandbox_enter_runs_local_instances_and_opens_the_add() {
    let mut stored = rhema_config::Profile::default();
    stored.place_sandbox_instance("dev", instance("docker", "rhema-dev"));
    let mut settings = sandbox_screen(
        offered(false, true, false),
        None,
        SandboxChoice::Local,
        stored,
    );
    // Enter on the host's own row selects local.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::SelectSandbox(SandboxChoice::Local)
    );
    // One down to the dev instance: enter reports the selection by
    // name; opening and running is the app's act on it.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::SelectSandboxInstance
    );
    assert_eq!(settings.selected_sandbox_instance(), Some("dev"));
    assert_eq!(settings.selected_sandbox(), None, "the instance row");
    // One more down lands on the add row.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.selected_sandbox(), None, "the add row");
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::OpenSandboxDialog
    );
    settings.sandbox_dialog = Some(AddSandboxDialog::new());
    // With the dialog up the rows stop answering: the dialog
    // takes every key while it is open, enter reporting the
    // submit for the app to act on.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::SubmitSandboxDialog
    );
}

#[test]
fn sandbox_d_reports_the_removal_and_local_and_add_stay_inert() {
    let mut stored = rhema_config::Profile::default();
    stored.place_sandbox_instance("dev", instance("docker", "rhema-dev"));
    let mut settings = sandbox_screen(
        offered(false, true, false),
        None,
        SandboxChoice::Local,
        stored,
    );
    // The host's own row has nothing to remove.
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::None
    );
    // One down to the dev instance: d reports the removal.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::RemoveSandboxInstance
    );
    // The add row stays inert.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::None
    );
}

#[test]
fn the_sandbox_dialog_picks_a_kind_and_stores_nothing_else() {
    let mut settings = sandbox_screen(
        SandboxAvailability::default(),
        None,
        SandboxChoice::Local,
        rhema_config::Profile::default(),
    );
    settings.sandbox_dialog = Some(AddSandboxDialog::new());
    // Arrows walk the ready kinds; enter reports the submit with
    // the kind picked — and no value, ever: the concrete name is
    // the workspace's to set, the app's to derive.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(
        settings
            .sandbox_dialog
            .as_ref()
            .expect("up")
            .selected_kind(),
        SandboxChoice::Lxd
    );
    settings.handle_key(key(KeyCode::Up));
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::SubmitSandboxDialog
    );
    // The screen keeps the dialog until the app acts on the
    // submit; esc closes it.
    assert!(settings.sandbox_dialog.is_some());
    settings.handle_key(key(KeyCode::Esc));
    assert!(settings.sandbox_dialog.is_none());
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

/// The submit a text-only input produces, the shape most tests
/// assert against.
fn submit(text: &str) -> ChatAction {
    ChatAction::Submit {
        text: text.to_owned(),
        images: Vec::new(),
    }
}

/// A profile holding one stored instance of `provider`, named after
/// it, carrying `key`.
fn keyed_profile(provider: &str, key: &str) -> rhema_config::Profile {
    let mut profile = rhema_config::Profile {
        provider: Some(provider.to_owned()),
        ..rhema_config::Profile::default()
    };
    profile.upsert_instance(
        provider,
        rhema_config::ProviderInstance {
            provider: provider.to_owned(),
            api_key: Some(key.to_owned()),
            secret_ref: None,
            base_url: None,
        },
    );
    profile
}

#[test]
fn the_model_description_names_the_vision_verdict_only_when_known() {
    let mut card = model("m", "Gemini", true);
    let description = |card: &ModelCard| model_description(card, &crate::icons::UNICODE);
    // Unknown says nothing: unknown is not a verdict.
    assert!(
        !description(&card).contains("vision"),
        "{}",
        description(&card)
    );
    card.supports_vision = Some(true);
    assert!(
        description(&card).contains("◉ vision"),
        "{}",
        description(&card)
    );
    card.supports_vision = Some(false);
    assert!(
        description(&card).contains("no vision"),
        "{}",
        description(&card)
    );
}

fn model(id: &str, display: &str, chat: bool) -> ModelCard {
    ModelCard {
        id: id.into(),
        display_name: display.into(),
        description: String::new(),
        input_token_limit: Some(1_048_576),
        output_token_limit: Some(65_536),
        supports_vision: None,
        generation_methods: if chat {
            vec!["generateContent".into()]
        } else {
            vec!["generateAudio".into()]
        },
        pricing: None,
    }
}

// --- Settings ---

fn settings_screen() -> SettingsScreen {
    let mut settings = SettingsScreen::new();
    settings.profile = "personal".into();
    settings
}

/// The dialog opened and descended to the credential line for the
/// provider at `row`, accepting the suggested name.
fn descend_to_credential(row: usize) -> (SettingsScreen, String) {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    for _ in 0..row {
        settings.handle_key(key(KeyCode::Down));
    }
    settings.handle_key(key(KeyCode::Enter));
    let name = settings.dialog.as_ref().unwrap().input.clone();
    settings.handle_key(key(KeyCode::Enter));
    (settings, name)
}

#[test]
fn q_leaves_the_settings_like_escape() {
    let mut settings = settings_screen();
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('q'))),
        SettingsAction::Back
    );
    // Inside the add dialog `q` is text, not a leave key: the
    // dialog takes every key while it is open.
    let (mut settings, _) = descend_to_credential(0);
    settings.handle_key(key(KeyCode::Char('q')));
    let dialog = settings.dialog.as_ref().expect("the dialog stands");
    assert!(dialog.input.ends_with('q'), "q is text: {:?}", dialog.input);
}

#[test]
fn settings_arrows_move_the_sidebar_and_wrap() {
    let mut settings = settings_screen();
    assert_eq!(settings.section, SettingsSection::Providers);
    // The horizontal keys and tabs move the sidebar; both ends wrap.
    settings.handle_key(key(KeyCode::Left));
    assert_eq!(
        settings.section,
        SettingsSection::About,
        "left wraps to the last section"
    );
    settings.handle_key(key(KeyCode::Right));
    assert_eq!(
        settings.section,
        SettingsSection::Providers,
        "right wraps back"
    );
    settings.handle_key(key(KeyCode::Tab));
    assert_eq!(settings.section, SettingsSection::Profiles);
    settings.handle_key(key(KeyCode::Tab));
    assert_eq!(settings.section, SettingsSection::Sandbox);
    settings.handle_key(key(KeyCode::Tab));
    assert_eq!(settings.section, SettingsSection::Appearance);
    settings.handle_key(key(KeyCode::Left));
    assert_eq!(settings.section, SettingsSection::Sandbox);
    settings.handle_key(key(KeyCode::Left));
    assert_eq!(settings.section, SettingsSection::Profiles);
    settings.handle_key(key(KeyCode::Left));
    assert_eq!(settings.section, SettingsSection::Providers);
    // The row selection restarts with the page.
    settings.row = 1;
    settings.handle_key(key(KeyCode::Right));
    assert_eq!(settings.row, 0);
}

#[test]
fn settings_rows_move_within_the_providers_page_and_wrap() {
    let mut settings = settings_screen();
    settings.stored = keyed_profile("google", "g-key");
    settings.stored.upsert_instance(
        "openrouter",
        rhema_config::ProviderInstance {
            provider: "openrouter".into(),
            api_key: Some("o-key".into()),
            secret_ref: None,
            base_url: None,
        },
    );
    // Rows: google, openrouter, the add row.
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 1);
    for _ in 0..5 {
        settings.handle_key(key(KeyCode::Down));
    }
    assert_eq!(settings.row, 0, "down wraps at the end");
    settings.handle_key(key(KeyCode::Up));
    assert_eq!(settings.row, 2, "up wraps to the add row");
}

#[test]
fn settings_enter_and_d_act_on_the_providers_rows() {
    let mut settings = settings_screen();
    settings.stored = keyed_profile("google", "g-key");
    // An instance row: enter activates, d forgets.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::ActivateInstance
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::RemoveInstance
    );
    // The add row: enter opens the dialog, d is nothing.
    settings.row = 1;
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::OpenDialog
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::None
    );
}

#[test]
fn settings_appearance_rows_toggle_and_carry_the_shortcuts() {
    let mut settings = settings_screen();
    settings.section = SettingsSection::Appearance;
    // Row 0 is the colors row; enter cycles the palette, c too.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::CyclePalette
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('c'))),
        SettingsAction::CyclePalette
    );
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 1);
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::ToggleTheme
    );
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 2);
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::ToggleIcons
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('t'))),
        SettingsAction::ToggleTheme
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('i'))),
        SettingsAction::ToggleIcons
    );
}

#[test]
fn settings_esc_backs_out_and_ctrl_combos_stay_with_the_app() {
    let mut settings = settings_screen();
    assert_eq!(settings.handle_key(key(KeyCode::Esc)), SettingsAction::Back);
    assert_eq!(
        settings.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        SettingsAction::None
    );
}

/// A Subagents page with the built-in and one workspace definition.
fn subagents_screen() -> SettingsScreen {
    let mut settings = settings_screen();
    settings.section = SettingsSection::Subagents;
    settings.subagents = vec![
        SubagentRow {
            name: "general-purpose".into(),
            description: "the built-in delegation".into(),
            model: None,
            max_steps: None,
            source: SubagentSource::Builtin,
            path: None,
        },
        SubagentRow {
            name: "reviewer".into(),
            description: "reviews diffs".into(),
            model: Some("zai/glm".into()),
            max_steps: Some(12),
            source: SubagentSource::Workspace,
            path: Some(PathBuf::from("/w/.rhema/subagents/reviewer/SUBAGENT.md")),
        },
    ];
    settings
}

#[test]
fn settings_subagent_rows_move_and_enter_edits() {
    let mut settings = subagents_screen();
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 1);
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 0, "down wraps past the last row");
    // Enter and `e` both edit the selected row.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::EditSubagent
    );
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('e'))),
        SettingsAction::EditSubagent
    );
}

#[test]
fn the_empty_subagents_page_has_nothing_to_act_on() {
    let mut settings = settings_screen();
    settings.section = SettingsSection::Subagents;
    assert_eq!(settings.selected_subagent(), None);
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::None
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('e'))),
        SettingsAction::None
    );
    settings.handle_key(key(KeyCode::Down));
    assert_eq!(settings.row, 0, "the arrows have nothing to move through");
}

#[test]
fn the_dialog_buffers_typing_and_masks_it() {
    let (mut settings, _) = descend_to_credential(0);
    for c in "sk-abc".chars() {
        settings.handle_key(key(KeyCode::Char(c)));
    }
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.input, "sk-abc");
    settings.handle_key(key(KeyCode::Backspace));
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.input, "sk-ab");
    assert_eq!(dialog.masked_input(), "\u{2022}".repeat(5));
    assert!(!dialog.masked_input().contains("sk"));
}

#[test]
fn the_dialog_enter_with_a_blank_credential_does_nothing() {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    settings.dialog.as_mut().unwrap().stage = DialogStage::EnterCredential;
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::None
    );
    assert_eq!(settings.dialog.as_ref().unwrap().status, DialogStatus::Idle);
    settings.dialog.as_mut().unwrap().input = "   ".into();
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::None
    );
}

#[test]
fn the_dialog_enter_validates_and_locks_until_the_outcome() {
    let (mut settings, _) = descend_to_credential(0);
    settings.dialog.as_mut().unwrap().input = " key ".into();
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::Validate
    );
    assert_eq!(
        settings.dialog.as_ref().unwrap().status,
        DialogStatus::Validating
    );
    // A validation in flight swallows Enter.
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::None
    );
    assert_eq!(
        settings.dialog.as_ref().unwrap().status,
        DialogStatus::Validating
    );
}

#[test]
fn the_copilot_row_starts_the_flow_and_esc_closes_the_dialog() {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    // Down to Copilot's row, then the name stage: enter starts the
    // flow, not a credential input.
    settings.dialog.as_mut().unwrap().selected = PROVIDERS.len() - 1;
    assert_eq!(
        settings.dialog.as_ref().unwrap().selected_provider(),
        LoginProvider::Copilot
    );
    settings.handle_key(key(KeyCode::Enter));
    assert_eq!(
        settings.dialog.as_ref().unwrap().stage,
        DialogStage::NameInstance
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::StartFlow
    );
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.stage, DialogStage::Flow);
    assert_eq!(dialog.status, DialogStatus::Validating);
    // A running flow swallows every key but esc.
    assert_eq!(
        settings.handle_key(key(KeyCode::Char('x'))),
        SettingsAction::None
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::None
    );
    // Esc cancels the flow and closes the dialog with it.
    assert_eq!(
        settings.handle_key(key(KeyCode::Esc)),
        SettingsAction::CloseDialog
    );
    assert!(settings.dialog.is_none());
}

#[test]
fn the_dialog_error_renders_inline_and_allows_retry() {
    let (mut settings, _) = descend_to_credential(0);
    let dialog = settings.dialog.as_mut().unwrap();
    dialog.input = "bad-key".into();
    dialog.status = DialogStatus::Validating;
    dialog.on_validated(&Err("gemini api error (HTTP 403): denied".into()));
    assert_eq!(
        dialog.status,
        DialogStatus::Error("gemini api error (HTTP 403): denied".into())
    );
    // Error state accepts Enter again (retry).
    assert_eq!(
        settings.handle_key(key(KeyCode::Enter)),
        SettingsAction::Validate
    );
}

#[test]
fn the_dialog_selection_wraps_across_the_providers() {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    for _ in 0..PROVIDERS.len() {
        settings.handle_key(key(KeyCode::Down));
    }
    assert_eq!(settings.dialog.as_ref().unwrap().selected, 0, "down wraps");
    settings.handle_key(key(KeyCode::Up));
    assert_eq!(
        settings.dialog.as_ref().unwrap().selected,
        PROVIDERS.len() - 1,
        "up wraps"
    );
}

#[test]
fn the_dialog_takes_a_masked_api_key_for_the_new_providers() {
    // The add flow: down three to Anthropic, the suggested name,
    // then the key.
    let (mut settings, _) = descend_to_credential(3);
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.selected_provider(), LoginProvider::Anthropic);
    assert_eq!(dialog.input_label(), "API key: ");
    for c in "sk-ant-01".chars() {
        settings.handle_key(key(KeyCode::Char(c)));
    }
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.input, "sk-ant-01");
    assert_eq!(dialog.masked_input(), "\u{2022}".repeat(9));
    assert!(
        !dialog.masked_input().contains("sk"),
        "no key material is rendered"
    );
}

#[test]
fn the_name_suggestion_skips_taken_names() {
    let mut stored = rhema_config::Profile::default();
    for name in ["openai", "openai-2"] {
        stored.instances.insert(
            name.to_owned(),
            rhema_config::ProviderInstance {
                provider: "openai".into(),
                api_key: Some("k".into()),
                secret_ref: None,
                base_url: None,
            },
        );
    }
    let mut settings = settings_screen();
    settings.stored = stored;
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    // Down to openai's row, then into the name input.
    for _ in 0..4 {
        settings.handle_key(key(KeyCode::Down));
    }
    settings.handle_key(key(KeyCode::Enter));
    assert_eq!(settings.dialog.as_ref().unwrap().input, "openai-3");
    // Renaming is free: clear and type.
    for _ in 0..8 {
        settings.handle_key(key(KeyCode::Backspace));
    }
    for c in "proxy".chars() {
        settings.handle_key(key(KeyCode::Char(c)));
    }
    settings.handle_key(key(KeyCode::Enter));
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(
        dialog.pending.as_ref().map(|p| p.name.as_str()),
        Some("proxy")
    );
    assert_eq!(dialog.stage, DialogStage::EnterCredential);
    // Esc walks back to the name with it intact.
    settings.handle_key(key(KeyCode::Esc));
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.stage, DialogStage::NameInstance);
    assert_eq!(dialog.input, "proxy");
}

#[test]
fn the_dialog_esc_walks_back_through_the_stages_then_closes() {
    let (mut settings, _) = descend_to_credential(0);
    settings.handle_key(key(KeyCode::Esc));
    assert_eq!(
        settings.dialog.as_ref().unwrap().stage,
        DialogStage::NameInstance
    );
    settings.handle_key(key(KeyCode::Esc));
    assert_eq!(
        settings.dialog.as_ref().unwrap().stage,
        DialogStage::SelectProvider
    );
    assert_eq!(
        settings.handle_key(key(KeyCode::Esc)),
        SettingsAction::CloseDialog
    );
    assert!(settings.dialog.is_none());
}

#[test]
fn a_click_can_pick_the_dialogs_provider_row() {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    settings.dialog.as_mut().unwrap().select_provider_row(3);
    let dialog = settings.dialog.as_ref().unwrap();
    assert_eq!(dialog.selected, 3);
    assert_eq!(dialog.selected_provider(), LoginProvider::Anthropic);
    // Out-of-range clicks clamp; other stages ignore them.
    settings.dialog.as_mut().unwrap().select_provider_row(99);
    assert_eq!(
        settings.dialog.as_ref().unwrap().selected,
        PROVIDERS.len() - 1
    );
    settings.handle_key(key(KeyCode::Enter));
    settings.dialog.as_mut().unwrap().select_provider_row(0);
    assert_eq!(
        settings.dialog.as_ref().unwrap().stage,
        DialogStage::NameInstance,
        "the name stage has no rows to pick"
    );
}

// --- Picker ---

fn catalog(provider: &'static str, models: Vec<ModelCard>) -> Vec<CatalogModel> {
    models
        .into_iter()
        .map(|model| CatalogModel {
            provider,
            instance: None,
            model,
        })
        .collect()
}

fn picker() -> PickerScreen {
    PickerScreen::new(catalog(
        "google",
        vec![
            model("gemini-3.8-pro", "Gemini 3.8 Pro", true),
            model("gemini-3.8-flash", "Gemini 3.8 Flash", true),
            // Rows without the chat marker are dropped by the
            // picker itself; the adapters never deliver text-wise
            // non-chat models in the first place.
            model("gemini-3.8-flash-tts", "Gemini 3.8 Flash TTS", false),
            model("text-embedding-1", "Text Embedding 1", false),
        ],
    ))
}

#[test]
fn picker_sorts_by_id() {
    let picker = picker();
    let ids: Vec<&str> = picker.models.iter().map(|m| m.model.id.as_str()).collect();
    assert_eq!(ids, ["gemini-3.8-flash", "gemini-3.8-pro"]);
}

#[test]
fn non_chat_models_are_dropped_from_the_list() {
    let picker = picker();
    let ids: Vec<&str> = picker.models.iter().map(|m| m.model.id.as_str()).collect();
    assert!(!ids.contains(&"gemini-3.8-flash-tts"));
    assert!(!ids.contains(&"text-embedding-1"));
    assert!(picker.models.iter().all(|m| m.provider == "google"));
}

#[test]
fn picker_filter_matches_id_or_display_name_case_insensitively() {
    let mut picker = picker();
    picker.filter = "FLASH".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|m| m.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-flash"]);

    picker.filter = "3.8 pro".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|m| m.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-pro"]);

    picker.filter = "zzz".into();
    assert!(picker.filtered().is_empty());
}

#[test]
fn the_filter_matches_the_rendered_provider_slash_id_row() {
    // A vendor slash in the filter names the provider's whole catalog
    // — `openrouter/` must not collapse to the models whose ids
    // happen to repeat the provider's name.
    let mut picker = PickerScreen::new(
        catalog(
            "openrouter",
            vec![
                model("openrouter/auto", "OpenRouter Auto", true),
                model("anthropic/claude", "Claude", true),
            ],
        )
        .into_iter()
        .chain(catalog(
            "google",
            vec![model("gemini-3.8-pro", "Gemini 3.8 Pro", true)],
        ))
        .collect(),
    );

    picker.filter = "openrouter/".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    // Every row renders `openrouter/…`, the provider's whole catalog.
    assert_eq!(ids, ["anthropic/claude", "openrouter/auto"]);

    picker.filter = "openrouter/anthropic".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    assert_eq!(ids, ["anthropic/claude"]);

    // And the bare provider still selects the whole group.
    picker.filter = "google".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-pro"]);
}

#[test]
fn typing_filters_without_any_prefix() {
    let mut picker = picker();
    assert_eq!(
        picker.handle_key(key(KeyCode::Char('p'))),
        PickerAction::None
    );
    picker.handle_key(key(KeyCode::Char('R')));
    assert_eq!(picker.filter, "pR");
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|m| m.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-pro"]);
}

#[test]
fn typing_filters_and_enter_chooses_the_highlighted_model() {
    let mut picker = picker();
    for c in "pro".chars() {
        picker.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        picker.handle_key(key(KeyCode::Enter)),
        PickerAction::Choose {
            provider: "google",
            instance: None,
            id: "gemini-3.8-pro".into(),
        }
    );
}

#[test]
fn the_filter_matches_the_provider_and_groups_are_sorted() {
    let mut picker = PickerScreen::new(
        catalog(
            "openrouter",
            vec![model("openrouter/auto", "OpenRouter Auto", true)],
        )
        .into_iter()
        .chain(catalog(
            "google",
            vec![model("gemini-3.8-flash", "Gemini 3.8 Flash", true)],
        ))
        .collect(),
    );

    // Unfiltered, the catalog groups by provider: google's rows first,
    // each prefixed with its own provider.
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-flash", "openrouter/auto"]);

    // The provider's name is itself a filter: one word picks its
    // whole catalog, whatever the model ids are.
    picker.filter = "openrouter".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    assert_eq!(ids, ["openrouter/auto"]);

    picker.filter = "google".into();
    let ids: Vec<&str> = picker
        .filtered()
        .iter()
        .map(|entry| entry.model.id.as_str())
        .collect();
    assert_eq!(ids, ["gemini-3.8-flash"]);
}

#[test]
fn arrows_navigate_while_typing_and_clamp_to_the_list() {
    let mut picker = picker();
    for c in "gemini".chars() {
        picker.handle_key(key(KeyCode::Char(c)));
    }
    for _ in 0..10 {
        picker.handle_key(key(KeyCode::Down));
    }
    let count = picker.filtered().len();
    assert_eq!(picker.selected, count - 1);
    picker.handle_key(key(KeyCode::Up));
    assert_eq!(picker.selected, count - 2);
    picker.handle_key(key(KeyCode::Up));
    picker.handle_key(key(KeyCode::Up));
    assert_eq!(picker.selected, 0, "up clamps at the top");
}

#[test]
fn backspace_undoes_the_filter() {
    let mut picker = picker();
    picker.handle_key(key(KeyCode::Char('t')));
    picker.handle_key(key(KeyCode::Char('t')));
    picker.handle_key(key(KeyCode::Backspace));
    assert_eq!(picker.filter, "t");
    picker.handle_key(key(KeyCode::Backspace));
    assert_eq!(picker.filter, "");
    assert_eq!(picker.filtered().len(), 2);
}

#[test]
fn esc_leaves_directly_even_with_a_filter() {
    let mut picker = picker();
    picker.handle_key(key(KeyCode::Char('t')));
    assert_eq!(
        picker.handle_key(key(KeyCode::Esc)),
        PickerAction::BackToChat
    );
    // The filter is transient; leaving does not bother with it.
    assert_eq!(picker.filter, "t");
}

#[test]
fn picker_enter_chooses_nothing_on_an_empty_list() {
    let mut picker = picker();
    picker.filter = "zzz".into();
    assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
}

#[test]
fn picker_esc_returns_to_the_chat() {
    // There is no startup gate anymore: a session without a model
    // opens on the chat, so the picker's Esc always goes back.
    let mut picker = picker();
    assert_eq!(
        picker.handle_key(key(KeyCode::Esc)),
        PickerAction::BackToChat
    );
    let mut empty = PickerScreen::new(Vec::new());
    assert_eq!(
        empty.handle_key(key(KeyCode::Esc)),
        PickerAction::BackToChat
    );
}

#[test]
fn picker_r_retries_only_from_the_error_state() {
    let mut picker = picker();
    assert_eq!(
        picker.handle_key(key(KeyCode::Char('r'))),
        PickerAction::None
    );
    picker.status = PickerStatus::Error("network error".into());
    assert_eq!(
        picker.handle_key(key(KeyCode::Char('r'))),
        PickerAction::Retry
    );
    assert_eq!(picker.status, PickerStatus::Loading);
}

#[test]
fn picker_set_models_replaces_and_clamps() {
    let mut picker = picker();
    picker.selected = 2;
    picker.set_models(catalog("google", vec![model("a-model", "A", true)]));
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.models[0].model.id, "a-model");
}

#[test]
fn picker_backspace_shrinks_the_filter() {
    let mut picker = picker();
    picker.handle_key(key(KeyCode::Char('a')));
    picker.handle_key(key(KeyCode::Char('b')));
    picker.handle_key(key(KeyCode::Backspace));
    assert_eq!(picker.filter, "a");
}

// --- Chat input ---

#[test]
fn chat_input_buffers_with_cursor_editing() {
    let mut chat = ChatState::new();
    for c in "helo".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    chat.handle_key(key(KeyCode::Left), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::Left), &[], &[], &[], &crate::icons::UNICODE);
    // cursor sits between "hel" and "o"
    chat.handle_key(
        key(KeyCode::Char('l')),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "hello");
    assert_eq!(chat.cursor, 3);
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "helo");
    chat.handle_key(key(KeyCode::End), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(
        key(KeyCode::Char('!')),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "helo!");
    chat.handle_key(key(KeyCode::Home), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::Delete), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, "elo!");
}

#[test]
fn chat_enter_submits_the_input_text() {
    let mut chat = ChatState::new();
    for c in "fix it".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        submit("fix it")
    );
    // The input box is NOT cleared here: the app clears it only when
    // it accepts the submit (a rejected submit — empty or mid-turn —
    // must not lose typed text).
    assert_eq!(chat.input, "fix it");
}

#[test]
fn alt_enter_and_ctrl_j_add_lines_plain_enter_submits() {
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    let mut chat = ChatState::new();
    chat.insert_str("x");
    chat.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "x\n");
    chat.handle_key(ctrl('j'), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, "x\n\n");
    assert_eq!(chat.cursor, 3);
    // Enter itself still submits the whole multi-line text.
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        submit("x\n\n")
    );
}

#[test]
fn a_paste_lands_on_the_dialogs_credential_line_without_control_characters() {
    let mut settings = settings_screen();
    settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
    // The list stage has no text line to fill.
    settings.dialog.as_mut().unwrap().paste("sk-123");
    assert_eq!(settings.dialog.as_ref().unwrap().input, "");

    // The add flow down to the credential line.
    settings.handle_key(key(KeyCode::Enter));
    settings.handle_key(key(KeyCode::Enter));
    settings.handle_key(key(KeyCode::Enter));
    settings.handle_key(key(KeyCode::Enter));
    settings.dialog.as_mut().unwrap().paste("sk-12\n34\r\n");
    assert_eq!(
        settings.dialog.as_ref().unwrap().input,
        "sk-1234",
        "a key is a single line"
    );
}

#[test]
fn altgr_composed_characters_insert_as_text() {
    // Outside the kitty protocol, AltGr reports CONTROL|ALT: on
    // French AZERTY, `@` is AltGr+0 and used to be swallowed as an
    // inert Ctrl combo.
    let mut chat = ChatState::new();
    chat.handle_key(
        KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "@");
    // Plain Ctrl combos stay inert.
    chat.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "@");
}

#[test]
fn a_multi_line_input_moves_the_cursor_between_lines() {
    let mut chat = ChatState::new();
    chat.insert_str("alpha\nbeta\ngamma");
    assert_eq!(chat.cursor_line_index(), 2, "the end is the last line");
    chat.handle_key(key(KeyCode::Up), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_line_index(), 1);
    assert_eq!(chat.cursor_column(), 4, "column kept");
    chat.handle_key(key(KeyCode::Up), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_line_index(), 0);
    assert_eq!(chat.cursor_column(), 4, "clamped to the shorter line");
    chat.handle_key(key(KeyCode::Up), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_line_index(), 0, "the top stays");
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_line_index(), 2);
}

#[test]
fn home_and_end_stay_within_the_cursor_line() {
    let mut chat = ChatState::new();
    chat.insert_str("alpha\nbeta");
    chat.handle_key(key(KeyCode::Home), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_line_index(), 1);
    assert_eq!(chat.cursor_column(), 0);
    chat.handle_key(key(KeyCode::End), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.cursor_column(), 4);
}

#[test]
fn a_long_paste_anchors_at_the_cursor_and_expands_there() {
    let mut chat = ChatState::new();
    let long = "l1\nl2\nl3\nl4";
    chat.insert_str("look at ");
    chat.paste(long);
    assert_eq!(chat.chips, vec![Chip::Paste(long.to_owned())]);
    assert_eq!(
        chat.input,
        format!("look at {}", ChatState::MARK),
        "the box shows a chip anchored at the paste, not the text"
    );

    // Enter submits the session form: the payload embedded at its
    // anchor, for the transcript to render as a chip and the
    // provider seam to expand.
    chat.insert_str(" closely");
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        submit(&format!(
            "look at {}{long}{} closely",
            ChatState::MARK,
            ChatState::MARK
        ))
    );
    assert_eq!(
        chat.input,
        format!("look at {} closely", ChatState::MARK),
        "declined keeps the text"
    );
    assert_eq!(chat.chips.len(), 1);
}

#[test]
fn a_pasted_image_mints_a_chip_and_rides_submit_beside_the_text() {
    let mut chat = ChatState::new();
    chat.insert_str("what is this? ");
    let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
    chat.paste_image(image.clone());
    assert_eq!(chat.input, format!("what is this? {}", ChatState::MARK));
    assert_eq!(chat.chips, vec![Chip::Image(image.clone())]);

    // Submit: the anchor drops out of the text and the image rides
    // beside it — an image has no inline form to embed into.
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        ChatAction::Submit {
            text: "what is this? ".into(),
            images: vec![image],
        }
    );
}

#[test]
fn interleaved_pastes_and_images_keep_each_kind_in_order() {
    let mut chat = ChatState::new();
    let first = "p1\np2\np3\np4";
    let second = "q1\nq2\nq3\nq4\nq5";
    chat.paste(first);
    chat.insert_str(" mid ");
    let image = rhema_core::Image::new(rhema_core::ImageMediaType::Jpeg, "d29ybGQ=");
    chat.paste_image(image.clone());
    chat.insert_str(" end");
    chat.paste(second);
    assert_eq!(
        chat.input.matches(ChatState::MARK).count(),
        3,
        "three anchors"
    );

    // Each paste embeds at its own anchor; the image's anchor drops
    // and the block keeps its place in the image list.
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        ChatAction::Submit {
            text: format!(
                // The image's anchor leaves only the spaces its
                // neighbours typed.
                "{m}{first}{m} mid  end{m}{second}{m}",
                m = ChatState::MARK
            ),
            images: vec![image],
        }
    );
}

#[test]
fn backspacing_an_image_chip_removes_it_alone() {
    let mut chat = ChatState::new();
    let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
    chat.paste_image(image);
    chat.paste("p1\np2\np3\np4");
    assert_eq!(chat.input.chars().count(), 2);

    // The backspace takes the last chip — the paste — and leaves
    // the image chip with its anchor.
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, ChatState::MARK.to_string());
    assert!(matches!(chat.chips.as_slice(), [Chip::Image(_)]));

    // And the image chip deletes like any other.
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert!(chat.chips.is_empty());
    assert_eq!(chat.input, "");
}

#[test]
fn backspacing_a_chip_removes_its_payload_with_it() {
    let mut chat = ChatState::new();
    let first = "l1\nl2\nl3\nl4";
    let second = "m1\nm2\nm3\nm4\nm5";
    chat.paste(first);
    chat.paste(second);
    assert_eq!(chat.input.chars().count(), 2, "two anchors");

    // Backspace removes the anchor behind the cursor — the last
    // chip — and its payload alone.
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.chips, vec![Chip::Paste(first.to_owned())]);
    assert_eq!(chat.input, ChatState::MARK.to_string());

    // Deleting the remaining anchor from before it works the same.
    chat.handle_key(key(KeyCode::Home), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::Delete), &[], &[], &[], &crate::icons::UNICODE);
    assert!(chat.chips.is_empty());
    assert_eq!(chat.input, "");
}

#[test]
fn a_short_paste_joins_the_input_inline() {
    let mut chat = ChatState::new();
    chat.paste("l1\nl2\nl3");
    assert!(chat.chips.is_empty());
    assert_eq!(chat.input, "l1\nl2\nl3");
}

#[test]
fn an_arriving_anchor_mark_never_mints_a_payloadless_chip() {
    let mut chat = ChatState::new();
    // A chip glyph copied back out of the box — via a terminal
    // selection or the clipboard — must not anchor without a
    // payload: the mark↔payload pairing is what backspace, delete,
    // and submit all index into.
    chat.paste(&format!("a{}b", ChatState::MARK));
    assert_eq!(chat.input, "ab");
    assert!(chat.chips.is_empty());
    chat.insert_str(&ChatState::MARK.to_string());
    assert_eq!(chat.input, "ab");

    // A terminal without bracketed paste delivers the same glyph as
    // a plain char key; it is dropped as well.
    chat.handle_key(
        KeyEvent::new(KeyCode::Char(ChatState::MARK), KeyModifiers::NONE),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "ab");

    // A real chip survives beside the stripped text: backspace
    // still removes the last payload, and nothing else.
    chat.paste("l1\nl2\nl3\nl4");
    assert_eq!(chat.input, format!("ab{}", ChatState::MARK));
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "ab");
    assert!(chat.chips.is_empty());
}

#[test]
fn shift_enter_adds_a_line() {
    let mut chat = ChatState::new();
    chat.insert_str("x");
    chat.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "x\n");
}

#[test]
fn a_shifted_letter_capitalizes_even_without_a_shifted_alternate() {
    let mut chat = ChatState::new();
    // A terminal reporting every key but not the shifted alternate
    // delivers the base char with SHIFT still set.
    chat.handle_key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "A");
    // Ghostty & co. send the shifted char with SHIFT cleared
    // (crossterm's alternate-key path): untouched.
    chat.handle_key(
        key(KeyCode::Char('B')),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "AB");
    // Plain lowercase typing stays lowercase.
    chat.handle_key(
        key(KeyCode::Char('c')),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "ABc");
}

#[test]
fn paste_normalizes_carriage_returns() {
    let mut chat = ChatState::new();
    chat.insert_str("a\r\nb\rc");
    assert_eq!(chat.input, "a\nb\nc");
}

#[test]
fn chat_enter_on_an_empty_input_yields_an_empty_submit() {
    let mut chat = ChatState::new();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE),
        submit("")
    );
}

#[test]
fn chat_scroll_moves_with_page_keys_and_never_goes_negative() {
    let mut chat = ChatState::new();
    chat.handle_key(key(KeyCode::PageUp), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::PageUp), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP);
    chat.scroll_by(-3);
    assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP - 3);
    chat.handle_key(
        key(KeyCode::PageDown),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    chat.handle_key(
        key(KeyCode::PageDown),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    chat.handle_key(
        key(KeyCode::PageDown),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.scroll, 0);
    chat.scroll_by(5);
    chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.scroll, 0, "submitting follows the latest line");
}

#[test]
fn the_command_table_lists_every_slash_command() {
    let names: Vec<&str> = crate::commands::COMMANDS
        .iter()
        .map(|spec| spec.name)
        .collect();
    assert!(names.contains(&"/profile"));
    assert!(names.contains(&"/model"));
    assert!(names.contains(&"/login"));
}

#[test]
fn slash_input_filters_the_command_palette() {
    let mut chat = ChatState::new();
    for c in "/".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    assert_eq!(
        crate::screens::filtered_commands("/").len(),
        crate::commands::COMMANDS.len()
    );
    for c in "cl".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    let names: Vec<&str> = crate::screens::filtered_commands("/cl")
        .iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names, ["/clear"]);
    // Not a command at all: nothing matches, nothing to complete.
    assert!(crate::screens::filtered_commands("/zzz").is_empty());
}

#[test]
fn arrows_navigate_the_palette_and_tab_completes() {
    let mut chat = ChatState::new();
    for c in "/".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.palette_index, 2);
    chat.handle_key(key(KeyCode::Up), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.palette_index, 1);
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, "/model");
    assert_eq!(chat.cursor, "/model".chars().count());
}

#[test]
fn the_model_palette_narrows_and_tab_fills_the_argument() {
    // Catalog order follows the fetch: providers alphabetical.
    let models = catalog(
        "google",
        vec![model("gemini-3.8-pro", "Gemini 3.8 Pro", true)],
    )
    .into_iter()
    .chain(catalog(
        "openrouter",
        vec![model("openrouter/auto", "OpenRouter Auto", true)],
    ))
    .collect::<Vec<_>>();
    let mut chat = ChatState::new();
    for c in "/model ".chars() {
        chat.handle_key(
            key(KeyCode::Char(c)),
            &models,
            &[],
            &[],
            &crate::icons::UNICODE,
        );
    }
    // The empty argument lists the whole catalog; the provider filter
    // narrows like the picker's.
    let ids: Vec<String> = crate::screens::palette_rows(
        &chat.input,
        chat.cursor,
        &models,
        &[],
        &[],
        &crate::icons::UNICODE,
    )
    .into_iter()
    .filter_map(|row| match row {
        crate::screens::PaletteRow::Model { id, .. } => Some(id),
        _ => None,
    })
    .collect();
    assert_eq!(ids, ["gemini-3.8-pro", "openrouter/auto"]);
    for c in "openrouter".chars() {
        chat.handle_key(
            key(KeyCode::Char(c)),
            &models,
            &[],
            &[],
            &crate::icons::UNICODE,
        );
    }
    let ids: Vec<String> = crate::screens::palette_rows(
        &chat.input,
        chat.cursor,
        &models,
        &[],
        &[],
        &crate::icons::UNICODE,
    )
    .into_iter()
    .filter_map(|row| match row {
        crate::screens::PaletteRow::Model { id, .. } => Some(id),
        _ => None,
    })
    .collect();
    assert_eq!(ids, ["openrouter/auto"]);
    // The vendor slash keeps narrowing instead of breaking the match.
    chat.handle_key(
        key(KeyCode::Char('/')),
        &models,
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    let ids: Vec<String> = crate::screens::palette_rows(
        &chat.input,
        chat.cursor,
        &models,
        &[],
        &[],
        &crate::icons::UNICODE,
    )
    .into_iter()
    .filter_map(|row| match row {
        crate::screens::PaletteRow::Model { id, .. } => Some(id),
        _ => None,
    })
    .collect();
    assert_eq!(ids, ["openrouter/auto"], "openrouter/ stays alive");
    // Tab completes the argument, keeping the command prefix.
    chat.handle_key(key(KeyCode::Tab), &models, &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, "/model openrouter/auto");
    assert_eq!(chat.cursor, "/model openrouter/auto".chars().count());
}

#[test]
fn the_thinking_palette_lists_its_levels_and_tab_fills_them() {
    let mut chat = ChatState::new();
    for c in "/thinking ".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    // The empty argument lists every level; a stem narrows it.
    let values = |chat: &ChatState| {
        crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &[],
            &[],
            &[],
            &crate::icons::UNICODE,
        )
        .into_iter()
        .filter_map(|row| match row {
            crate::screens::PaletteRow::Choice { value, .. } => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>()
    };
    assert_eq!(values(&chat), ["low", "medium", "high", "default"]);
    for c in "hi".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    assert_eq!(values(&chat), ["high"]);
    // Tab completes the level, keeping the command prefix.
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, "/thinking high");
    assert_eq!(chat.cursor, "/thinking high".chars().count());
    // A stem no level answers leaves the palette empty.
    let mut chat = ChatState::new();
    for c in "/thinking zzz".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    assert!(
        crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &[],
            &[],
            &[],
            &crate::icons::UNICODE
        )
        .is_empty()
    );
    // Without the argument's space the command row itself matches,
    // not the levels.
    let mut chat = ChatState::new();
    for c in "/think".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    assert!(matches!(
        crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &[],
            &[],
            &[],
            &crate::icons::UNICODE
        ).as_slice(),
        [crate::screens::PaletteRow::Command(spec)] if spec.name == "/thinking"
    ));
}

#[test]
fn the_profile_palette_narrows_and_tab_fills_the_argument() {
    let profiles = vec![
        PaletteProfile {
            name: "personal".into(),
            provider: Some("google".into()),
            model: Some("gemini-3.8-flash".into()),
            active: true,
        },
        PaletteProfile {
            name: "work".into(),
            provider: Some("openrouter".into()),
            model: None,
            active: false,
        },
    ];
    let mut chat = ChatState::new();
    for c in "/profile wo".chars() {
        chat.handle_key(
            key(KeyCode::Char(c)),
            &[],
            &profiles,
            &[],
            &crate::icons::UNICODE,
        );
    }
    // The name narrows; so does the provider text a row carries.
    let names = |input: &str| {
        crate::screens::palette_rows(
            input,
            input.chars().count(),
            &[],
            &profiles,
            &[],
            &crate::icons::UNICODE,
        )
        .into_iter()
        .filter_map(|row| match row {
            crate::screens::PaletteRow::Profile { name, .. } => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>()
    };
    assert_eq!(names(&chat.input), ["work"]);
    assert_eq!(names("/profile openrouter"), ["work"]);
    assert_eq!(names("/profile google").len(), 1, "personal matches too");
    // Tab completes the argument under the command prefix.
    chat.input = "/profile wo".to_owned();
    chat.cursor = chat.input.chars().count();
    chat.handle_key(
        key(KeyCode::Tab),
        &[],
        &profiles,
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "/profile work");
}

#[test]
fn the_sandbox_palette_describes_instances_and_matches_their_text() {
    let sandboxes = vec![
        PaletteSandbox {
            name: "dev".into(),
            kind: "docker".into(),
            target: Some("rhema-dev".into()),
            active: true,
        },
        PaletteSandbox {
            name: "build".into(),
            kind: "lxd".into(),
            target: None,
            active: false,
        },
    ];
    let rows = |input: &str| {
        crate::screens::palette_rows(
            input,
            input.chars().count(),
            &[],
            &[],
            &sandboxes,
            &crate::icons::UNICODE,
        )
    };
    // The active instance's row says so; the kind and target ride
    // the description.
    let (value, description) = match &rows("/sandbox ")[0] {
        PaletteRow::Sandbox { value, description } => (value.clone(), description.clone()),
        other => panic!("expected a sandbox row, got {other:?}"),
    };
    assert_eq!(value, "dev");
    assert_eq!(description, "docker · rhema-dev · active");
    // A bare instance with no target names only its kind.
    let (_, description) = match &rows("/sandbox ")[1] {
        PaletteRow::Sandbox { value, description } => (value.clone(), description.clone()),
        other => panic!("expected a sandbox row, got {other:?}"),
    };
    assert_eq!(description, "lxd");
    // The kind text completes the instance too, the way the picker
    // filters.
    let values: Vec<String> = rows("/sandbox lxd")
        .into_iter()
        .filter_map(|row| match row {
            PaletteRow::Sandbox { value, .. } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(values, ["build"]);
}

// --- Path completion ---

/// A directory holding `sub/` (empty), `alpha.txt`, `Beta.txt`, and
/// the hidden `.hidden`, for the path-completion tests.
fn path_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir stands");
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
    std::fs::write(dir.path().join("alpha.txt"), "").expect("write");
    std::fs::write(dir.path().join("Beta.txt"), "").expect("write");
    std::fs::write(dir.path().join(".hidden"), "").expect("write");
    dir
}

/// The path rows' labels for `input` with the cursor at its end.
fn path_names(input: &str) -> Vec<String> {
    crate::screens::palette_rows(
        input,
        input.chars().count(),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    )
    .into_iter()
    .filter_map(|row| match row {
        crate::screens::PaletteRow::Path { name, .. } => Some(name),
        _ => None,
    })
    .collect()
}

#[test]
fn the_path_token_is_the_word_under_the_cursor() {
    let input = "look at src/al and stop";
    let cursor = "look at src/al".chars().count();
    assert_eq!(path_token(input, cursor), Some((8, 14)));
    assert_eq!(
        path_token(input, 10),
        Some((8, 14)),
        "mid-token is the same token"
    );
    assert_eq!(path_token(input, 0), Some((0, 4)));
    // The cursor on the break still picks the word to its left.
    assert_eq!(path_token(input, 14), Some((8, 14)));
    // An anchor splits words: a chip is never part of a token.
    let marked = format!("a{}b src", ChatState::MARK);
    assert_eq!(path_token(&marked, marked.chars().count()), Some((4, 7)));
    assert_eq!(
        path_token(&marked, 2),
        Some((2, 3)),
        "an anchor splits words"
    );
    assert_eq!(path_token("see ", 4), None);
}

#[test]
fn the_palette_lists_the_entries_the_token_names() {
    let dir = path_fixture();
    let base = dir.path().display().to_string();
    assert_eq!(path_names(&format!("see {base}/a")), ["alpha.txt"]);
    // Case-insensitive, like every other palette filter.
    assert_eq!(path_names(&format!("see {base}/b")), ["Beta.txt"]);
    // An empty stem lists the directory: directories first, then
    // names case-insensitively, and no hidden entries.
    assert_eq!(
        path_names(&format!("see {base}/")),
        ["sub/", "alpha.txt", "Beta.txt"]
    );
    // A stem leading with a dot unhides them.
    assert_eq!(path_names(&format!("see {base}/.")), [".hidden"]);
}

#[test]
fn plain_words_complete_nothing() {
    let _dir = path_fixture();
    assert!(path_names("hello world").is_empty());
    // A file name without a separator is prose, not a path.
    assert!(path_names("see alpha.txt").is_empty());
    assert!(path_names("").is_empty());
}

#[test]
fn tab_splices_the_entry_over_the_token() {
    let dir = path_fixture();
    let base = dir.path().display().to_string();
    let mut chat = ChatState::new();
    // A file completes in place.
    chat.input = format!("see {base}/b");
    chat.cursor = chat.input.chars().count();
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, format!("see {base}/Beta.txt"));
    assert_eq!(chat.cursor, chat.input.chars().count());
    // A directory gains its separator; the next Tab lists inside it.
    chat.input = format!("see {base}/su");
    chat.cursor = chat.input.chars().count();
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(chat.input, format!("see {base}/sub/"));
    assert_eq!(chat.cursor, chat.input.chars().count());
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(
        chat.input,
        format!("see {base}/sub/"),
        "an empty directory lists nothing"
    );
}

#[test]
fn completing_a_path_leaves_other_attachments_alone() {
    let dir = path_fixture();
    let base = dir.path().display().to_string();
    let mut chat = ChatState::new();
    chat.paste("p\np\np\np");
    assert_eq!(chat.chips.len(), 1);
    for c in format!(" see {base}/b").chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &crate::icons::UNICODE);
    assert_eq!(
        chat.input,
        format!("{} see {base}/Beta.txt", ChatState::MARK)
    );
    assert_eq!(chat.chips.len(), 1, "the chip survives");
}

#[test]
fn slash_commands_and_arguments_outrank_path_completion() {
    let dir = path_fixture();
    let base = dir.path().display().to_string();
    // "/" lists commands, never root's entries.
    let rows = crate::screens::palette_rows("/", 1, &[], &[], &[], &crate::icons::UNICODE);
    assert!(rows.iter().all(|row| matches!(row, PaletteRow::Command(_))));
    // A `/model` argument with no catalog match falls through to
    // path completion.
    let input = format!("/model {base}/al");
    let rows = crate::screens::palette_rows(
        &input,
        input.chars().count(),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert!(matches!(&rows[0], PaletteRow::Path { .. }));
}

#[test]
fn editing_the_input_resets_the_palette_selection() {
    let mut chat = ChatState::new();
    for c in "/".chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &crate::icons::UNICODE);
    }
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &crate::icons::UNICODE);
    chat.handle_key(
        key(KeyCode::Backspace),
        &[],
        &[],
        &[],
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.palette_index, 0);
}

#[test]
fn chat_ctrl_combos_are_left_to_the_app() {
    let mut chat = ChatState::new();
    assert_eq!(
        chat.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &[],
            &[],
            &[],
            &crate::icons::UNICODE,
        ),
        ChatAction::None
    );
    assert_eq!(chat.input, "");
}

fn two_profiles() -> BTreeMap<String, rhema_config::Profile> {
    BTreeMap::from([
        ("personal".to_owned(), keyed_profile("google", "k")),
        ("work".to_owned(), rhema_config::Profile::default()),
    ])
}

#[test]
fn the_profiles_page_moves_over_the_add_row_and_acts() {
    let profiles = two_profiles();
    let mut screen = crate::screens::SettingsScreen::new();
    screen.section = SettingsSection::Profiles;
    screen.profiles = profile_rows(&profiles, Some("personal"), None);
    // Rows: personal, work, add. Down walks them and wraps.
    assert_eq!(screen.selected_profile(), Some("personal"));
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(screen.selected_profile(), Some("work"));
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(screen.selected_profile(), None, "the add row is no profile");
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(screen.selected_profile(), Some("personal"), "the rows wrap");
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SettingsAction::SelectProfile
    );
    // Up from the top wraps onto the add row, whose enter opens the
    // dialog; esc then closes it.
    screen.handle_key(key(KeyCode::Up));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SettingsAction::OpenProfileDialog
    );
    screen.add_profile = Some(AddProfileDialog::new(&profiles));
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        SettingsAction::CloseProfileDialog
    );
    assert!(screen.add_profile.is_none(), "esc closed the dialog");
    // d and c answer only on a profile row.
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::None,
        "the add row removes nothing"
    );
    screen.handle_key(key(KeyCode::Up));
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::RemoveProfile
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('c'))),
        SettingsAction::CycleProfileColor
    );
    // The other pages are untouched: selected_profile is page-bound.
    screen.section = SettingsSection::Providers;
    assert_eq!(screen.selected_profile(), None);
}

#[test]
fn the_add_profile_dialog_edits_refuses_and_closes() {
    let profiles = two_profiles();
    let mut dialog = AddProfileDialog::new(&profiles);
    // Blank names never submit.
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        ProfileDialogAction::None
    );
    for c in "  side ".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(dialog.name(), "side", "the name trims");
    // A taken name refuses and stays up, the error named.
    dialog.input.clear();
    for c in "work".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        ProfileDialogAction::None
    );
    assert!(dialog.error.is_some(), "the taken name names the error");
    // Editing clears the error; a fresh name submits.
    dialog.handle_key(key(KeyCode::Backspace));
    assert_eq!(dialog.error, None);
    for c in "side".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        ProfileDialogAction::Submit
    );
    // Esc closes; ctrl combos never reach the input, and a paste's
    // control characters drop.
    assert_eq!(
        dialog.handle_key(key(KeyCode::Esc)),
        ProfileDialogAction::Close
    );
    dialog.input.clear();
    dialog.paste("a\nb\u{7}c");
    assert_eq!(dialog.input, "abc");
    assert_eq!(
        dialog.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        ProfileDialogAction::None
    );
}

// --- MCP page ---

/// A stdio entry under `name`, with one env pair.
fn stdio_entry(description: &str, env: &[(&str, &str)]) -> rhema_config::mcp::McpServerEntry {
    rhema_config::mcp::McpServerEntry {
        description: description.to_owned(),
        command: Some(vec!["npx".to_owned(), "-y".to_owned(), "server".to_owned()]),
        url: None,
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        headers: BTreeMap::new(),
        startup_timeout: None,
        timeout: None,
        command_name: None,
    }
}

/// A http entry under a url and one header.
fn http_entry(description: &str) -> rhema_config::mcp::McpServerEntry {
    rhema_config::mcp::McpServerEntry {
        description: description.to_owned(),
        command: None,
        url: Some("http://127.0.0.1:9721/mcp".to_owned()),
        env: BTreeMap::new(),
        headers: BTreeMap::from([("Authorization".to_owned(), "Bearer t".to_owned())]),
        startup_timeout: None,
        timeout: None,
        command_name: None,
    }
}

/// A settings screen on the MCP page over `entries`, with `disabled`
/// named muted.
fn mcp_screen(
    entries: BTreeMap<String, rhema_config::mcp::McpServerEntry>,
    disabled: &[&str],
) -> SettingsScreen {
    let mut settings = SettingsScreen::new();
    settings.section = SettingsSection::Mcp;
    settings.mcp = crate::screens::mcp_server_rows(
        &entries,
        &disabled.iter().map(|n| (*n).to_owned()).collect(),
    );
    settings
}

#[test]
fn the_mcp_screen_lists_the_active_profile_s_servers() {
    // An empty map: no rows, the add row is the only target, and the
    // renderer's hint carries the add nudge.
    let empty = mcp_screen(BTreeMap::new(), &[]);
    assert!(empty.mcp.is_empty());
    assert_eq!(empty.selected_mcp_server(), None);
    // Two servers in name order, each carrying its transport.
    let entries = BTreeMap::from([
        (
            "atlassian".to_owned(),
            stdio_entry("Jira and Confluence", &[("TOKEN", "t")]),
        ),
        ("blender".to_owned(), http_entry("Blender scene operations")),
    ]);
    let settings = mcp_screen(entries.clone(), &["atlassian"]);
    assert_eq!(
        settings
            .mcp
            .iter()
            .map(|row| (row.name.as_str(), row.transport))
            .collect::<Vec<_>>(),
        vec![("atlassian", Some("stdio")), ("blender", Some("http"))],
        "the map's name order, the transport tagged"
    );
    assert_eq!(settings.mcp[0].description, "Jira and Confluence");
    assert!(settings.mcp[0].disabled, "the muted name is marked");
    assert!(!settings.mcp[1].disabled);
    assert_eq!(settings.selected_mcp_server(), Some("atlassian"));
    // A broken entry still lists, carrying the notice the assembly
    // would report — the row text is from_entries' own wording.
    let mut broken = entries;
    broken.insert(
        "both".to_owned(),
        rhema_config::mcp::McpServerEntry {
            description: "both transports named".to_owned(),
            command: Some(vec!["true".to_owned()]),
            url: Some("http://127.0.0.1:1/mcp".to_owned()),
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            startup_timeout: None,
            timeout: None,
            command_name: None,
        },
    );
    let rows = mcp_server_rows(&broken, &BTreeSet::new());
    let both = rows.iter().find(|row| row.name == "both").unwrap();
    assert_eq!(both.transport, None, "neither transport wins");
    assert_eq!(
        both.notice,
        rhema_config::mcp::McpConfig::from_entries(&broken)
            .notices
            .iter()
            .find(|n| n.contains("`both`"))
            .cloned(),
        "the row quotes the fold's notice"
    );
    assert!(rows[0].notice.is_none(), "a kept entry shows no notice");
}

#[test]
fn the_mcp_page_moves_over_the_rows_and_acts() {
    let entries = BTreeMap::from([
        ("atlassian".to_owned(), stdio_entry("Jira", &[])),
        ("blender".to_owned(), http_entry("Blender")),
    ]);
    let mut screen = mcp_screen(entries, &[]);
    // Rows: atlassian, blender, add. Down walks them and wraps.
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(screen.selected_mcp_server(), Some("blender"));
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(screen.selected_mcp_server(), None, "the add row");
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(
        screen.selected_mcp_server(),
        Some("atlassian"),
        "the rows wrap"
    );
    // Enter on a server row mutes it; the add row's enter opens the
    // dialog, which esc then closes.
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SettingsAction::ToggleMcpServer
    );
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Down));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SettingsAction::OpenMcpDialog
    );
    screen.mcp_dialog = Some(AddMcpServerDialog::new(BTreeSet::new()));
    assert_eq!(screen.selected_mcp_server(), None, "the dialog blocks");
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        SettingsAction::CloseMcpDialog
    );
    assert!(screen.mcp_dialog.is_none(), "esc closed the dialog");
    // Space toggles like enter, the skills page's pair.
    screen.handle_key(key(KeyCode::Up));
    assert_eq!(
        screen.handle_key(key(KeyCode::Char(' '))),
        SettingsAction::ToggleMcpServer
    );
    // d arms the removal confirm; esc cancels it and stays on the
    // page, y confirms and the action carries.
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('d'))),
        SettingsAction::None,
        "d only arms the confirm"
    );
    assert_eq!(screen.mcp_confirm.as_deref(), Some("blender"));
    assert_eq!(screen.selected_mcp_server(), None, "the confirm blocks");
    screen.handle_key(key(KeyCode::Esc));
    assert_eq!(screen.mcp_confirm, None, "esc left the row alone");
    screen.handle_key(key(KeyCode::Char('d')));
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('n'))),
        SettingsAction::None
    );
    screen.handle_key(key(KeyCode::Char('d')));
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('y'))),
        SettingsAction::RemoveMcpServer
    );
    assert_eq!(screen.mcp_confirm, None, "the confirm spent itself");
    // The other pages are untouched: selected_mcp_server is page-bound.
    screen.section = SettingsSection::Providers;
    assert_eq!(screen.selected_mcp_server(), None);
}

#[test]
fn the_add_mcp_dialog_rejects_bad_names_and_both_transports() {
    let mut dialog = AddMcpServerDialog::new(BTreeSet::from(["taken".to_owned()]));
    // Blank and badly-cased names stay on the stage, the error named.
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    for c in "My Server".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    assert!(
        dialog.error.is_some(),
        "the charset refuses: {:?}",
        dialog.error
    );
    dialog.input.clear();
    for c in "taken".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    assert!(
        dialog
            .error
            .as_deref()
            .is_some_and(|e| e.contains("already exists")),
        "the taken name refuses: {:?}",
        dialog.error
    );
    // A fresh name lands on the description stage, which wants text.
    dialog.input.clear();
    dialog.error = None;
    for c in "fresh".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    assert_eq!(dialog.name, "fresh");
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    // The description is optional: an empty one advances, and the
    // fold substitutes the "--help lists its tools" line.
    assert_eq!(dialog.stage, McpDialogStage::Transport);
    assert_eq!(dialog.description, "");
    // Esc back, type one, and it lands too.
    dialog.handle_key(key(KeyCode::Esc));
    assert_eq!(dialog.stage, McpDialogStage::Description);
    for c in "Jira".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::Transport);
    assert_eq!(dialog.description, "Jira");
    // An empty argv and an empty url both hold their stage.
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::Command);
    dialog.handle_key(key(KeyCode::Enter));
    assert!(dialog.error.is_some(), "the empty command holds");
    dialog.handle_key(key(KeyCode::Esc));
    dialog.handle_key(key(KeyCode::Down));
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::Url);
    dialog.handle_key(key(KeyCode::Enter));
    assert!(dialog.error.is_some(), "the empty url holds");
    // The belt: an entry with both transports rejects, whatever the
    // stages would have allowed.
    dialog.url = Some("http://127.0.0.1:1/mcp".to_owned());
    dialog.command = Some(vec!["true".to_owned()]);
    let built = dialog.build();
    assert!(
        built.as_ref().is_err_and(|e| e.contains("exclusive")),
        "both transports reject: {built:?}"
    );
    // So does a command_name that is no bash identifier.
    dialog.command = Some(vec!["true".to_owned()]);
    dialog.url = None;
    dialog.command_name = "2bad".to_owned();
    assert!(
        dialog
            .build()
            .err()
            .is_some_and(|e| e.contains("bash identifier")),
        "{:?}",
        dialog.build()
    );
    // Malformed and duplicated env lines reject at the stage's exit.
    dialog.command_name.clear();
    dialog.stage = McpDialogStage::EnvLines;
    dialog.env = PairLines {
        lines: vec!["TOKEN".to_owned(), String::new()],
        cursor: 1,
    };
    dialog.handle_key(key(KeyCode::Enter));
    assert!(
        dialog
            .error
            .as_deref()
            .is_some_and(|e| e.contains("KEY=value")),
        "the keyless line holds: {:?}",
        dialog.error
    );
    // One key said twice holds the stage at its exit.
    dialog.env = PairLines {
        lines: vec!["TOKEN=a".to_owned(), "TOKEN=b".to_owned(), String::new()],
        cursor: 2,
    };
    dialog.handle_key(key(KeyCode::Enter));
    assert!(
        dialog.error.as_deref().is_some_and(|e| e.contains("twice")),
        "the doubled key holds: {:?}",
        dialog.error
    );
}

#[test]
fn the_add_mcp_dialog_walks_the_stdio_flow_and_builds_the_entry() {
    let mut dialog = AddMcpServerDialog::new(BTreeSet::new());
    for c in "atlassian".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    for c in "Jira and Confluence".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    // stdio is the first row: enter takes it and the command stage.
    dialog.handle_key(key(KeyCode::Enter));
    for c in "npx -y @mcp/server".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::EnvLines);
    // One env pair, then enter on the blank line moves on.
    for c in "JIRA_TOKEN=hunter2".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None
    );
    assert_eq!(dialog.stage, McpDialogStage::CommandName);
    // Esc steps back to the lines, then forward again; the empty
    // override stores.
    dialog.handle_key(key(KeyCode::Esc));
    assert_eq!(dialog.stage, McpDialogStage::EnvLines);
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::Submit
    );
    let (name, entry) = dialog.build().expect("the entry builds");
    assert_eq!(name, "atlassian");
    assert_eq!(entry.description, "Jira and Confluence");
    assert_eq!(
        entry.command,
        Some(vec![
            "npx".to_owned(),
            "-y".to_owned(),
            "@mcp/server".to_owned()
        ])
    );
    assert_eq!(
        entry.env.get("JIRA_TOKEN").map(String::as_str),
        Some("hunter2")
    );
    assert_eq!(entry.url, None);
    assert_eq!(entry.command_name, None, "empty means none");
    assert_eq!(entry.resolve(&name).map(|_| ()), Ok(()));
}

#[test]
fn the_add_mcp_dialog_walks_the_http_flow_with_headers() {
    let mut dialog = AddMcpServerDialog::new(BTreeSet::new());
    for c in "blender".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    for c in "Blender scene operations".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    // Down to the http row, then in.
    dialog.handle_key(key(KeyCode::Down));
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::Url);
    for c in "http://127.0.0.1:9721/mcp".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    assert_eq!(dialog.stage, McpDialogStage::HeaderLines);
    // Two header pairs, each enter opening the next line, and the
    // blank last line's enter finishing the stage.
    for c in "Authorization=Bearer t".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    dialog.handle_key(key(KeyCode::Enter));
    for c in "X-Tenant=acme".chars() {
        dialog.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None,
        "the second pair opened a line"
    );
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::None,
        "the blank line finished the stage"
    );
    assert_eq!(dialog.stage, McpDialogStage::CommandName);
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        McpDialogAction::Submit
    );
    let (_, entry) = dialog.build().expect("the entry builds");
    assert_eq!(entry.url.as_deref(), Some("http://127.0.0.1:9721/mcp"));
    assert_eq!(entry.command, None);
    assert_eq!(
        entry.headers.get("Authorization").map(String::as_str),
        Some("Bearer t")
    );
    assert_eq!(
        entry.headers.get("X-Tenant").map(String::as_str),
        Some("acme")
    );
    assert_eq!(entry.env, BTreeMap::new(), "headers servers carry no env");
    // A paste lands on the dialog's inputs, control characters and
    // all dropped.
    dialog.stage = McpDialogStage::CommandName;
    dialog.paste("na\nme\u{7}");
    assert_eq!(dialog.input, "name");
    assert_eq!(
        dialog.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        McpDialogAction::None
    );
}
