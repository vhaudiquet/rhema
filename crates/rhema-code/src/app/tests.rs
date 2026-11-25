//! The app's behavior tests: the `impl App` slices live beside the
//! code they exercise, the tests share them from here.
use super::*;
use crate::events::OwnedEvent;
use crate::screens::{LoginProvider, ModelPicker, ProviderCategory, providers_in};
use crate::store::MemoryStore;
use crate::transcript::Line;
use rhema_config::secrets::SecretStore as _;
use rhema_core::{Message, Role, Session, StopReason, ToolCall, Usage};
use rhema_host_agent::{Approval, HostAgentConfig};
use rhema_provider::ModelCard;
use std::collections::BTreeMap;
use std::num::NonZeroU32;

/// A turn provider the cancelled-turn tests never drive, these tests
/// exercise the UI's cancellation bookkeeping, not an exchange, so any
/// `Arc<dyn ModelProvider>` will do and a scripted one (feature
/// `mock`) would gate the tests.
struct NeverDriven;

#[async_trait::async_trait]
impl rhema_core::ModelProvider for NeverDriven {
    async fn complete(
        &self,
        _req: rhema_core::ModelRequest,
        _sink: &mut dyn rhema_core::EventSink,
    ) -> Result<rhema_core::ModelOutput, rhema_core::ProviderError> {
        Err(rhema_core::ProviderError::Network(
            "these tests never complete a turn".to_owned(),
        ))
    }
}

/// A described skill folder under `dir`, the way discovery reads
/// one.
fn write_skill(dir: &Path, name: &str, description: &str) -> PathBuf {
    let folder = dir.join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("SKILL.md");
    std::fs::write(
        &file,
        format!("---\ndescription: {description}\n---\n\nBody.\n"),
    )
    .unwrap();
    file
}

#[test]
fn skill_rows_name_their_source_and_the_rhema_copy_hides_the_agents_one() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let agents = home.path().join(".agents").join("skills");
    let global = home.path().join(".config").join("rhema").join("skills");
    write_skill(&agents, "borrowed", "the agents trigger");
    // A rhema-global copy and an .agents copy share one name. The
    // page shows one row, tagged global, and the hidden copy never
    // lists.
    write_skill(&agents, "shared", "agents trigger");
    write_skill(&global, "shared", "global trigger");
    write_skill(
        &workspace.path().join(".rhema").join("skills"),
        "local",
        "workspace trigger",
    );

    let disabled = std::collections::BTreeSet::new();
    let rows = skill_rows(Some(&agents), Some(&global), workspace.path(), &disabled);
    let tagged = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no {name} row in {rows:?}"))
            .source
    };
    assert_eq!(rows.len(), 3, "the shared .agents copy hides: {rows:?}");
    assert_eq!(tagged("local"), SkillSource::Workspace);
    assert_eq!(tagged("shared"), SkillSource::Global);
    assert_eq!(tagged("borrowed"), SkillSource::Agents);
}

#[test]
fn a_workspace_skill_of_an_agents_name_shadows_to_workspace() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let agents = home.path().join(".agents").join("skills");
    write_skill(&agents, "shared", "agents trigger");
    write_skill(
        &workspace.path().join(".rhema").join("skills"),
        "shared",
        "workspace trigger",
    );

    let rows = skill_rows(
        Some(&agents),
        None,
        workspace.path(),
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, SkillSource::Workspace);
    assert_eq!(rows[0].description, "workspace trigger");
}

#[test]
fn a_workspace_agents_skill_is_tagged_agents() {
    let workspace = tempfile::tempdir().unwrap();
    write_skill(
        &workspace.path().join(".agents").join("skills"),
        "installed",
        "installed trigger",
    );

    let rows = skill_rows(
        None,
        None,
        workspace.path(),
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, SkillSource::Agents);
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn model(id: &str) -> ModelCard {
    ModelCard {
        id: id.into(),
        display_name: id.into(),
        description: String::new(),
        input_token_limit: Some(1_048_576),
        output_token_limit: Some(65_536),
        supports_vision: None,
        generation_methods: vec!["generateContent".into()],
        pricing: None,
        family: None,
        release_date: None,
    }
}

fn models() -> Vec<ModelCard> {
    vec![model("m-b"), model("m-a")]
}

/// A `ModelsLoaded` result of one provider's catalog, the shape the
/// fetch task reports per queried backend. The instance is named
/// after the provider, the way a migrated login lands.
fn loaded(provider: &'static str, models: Vec<ModelCard>) -> Vec<crate::events::CatalogFetch> {
    vec![crate::events::CatalogFetch {
        provider,
        instance: Some(provider.to_owned()),
        result: Ok(models),
    }]
}

/// A `ModelsLoaded` result of one provider's failure.
fn failed(provider: &'static str, message: &str) -> Vec<crate::events::CatalogFetch> {
    vec![crate::events::CatalogFetch {
        provider,
        instance: Some(provider.to_owned()),
        result: Err(crate::events::CatalogFailure::Error(message.to_owned())),
    }]
}

/// A `ModelsLoaded` result of one provider that did not answer.
fn unavailable(provider: &'static str) -> Vec<crate::events::CatalogFetch> {
    vec![crate::events::CatalogFetch {
        provider,
        instance: Some(provider.to_owned()),
        result: Err(crate::events::CatalogFailure::Unavailable),
    }]
}

/// A priced card, for cost-math tests: the price rides the row, the
/// way the adapters hand catalogs out.
fn priced_model(id: &str, pricing: rhema_provider::Pricing) -> ModelCard {
    ModelCard {
        pricing: Some(pricing),
        ..model(id)
    }
}

/// A llama.cpp row: an id with none of the metadata Google reports.
fn llama_model(id: &str) -> ModelCard {
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

fn test_call() -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        args: serde_json::json!({ "command": "ls" }),
        provider_data: serde_json::Value::Null,
    }
}

struct Harness {
    app: App,
    store: Arc<MemoryStore>,
    /// The store the app places logins in, when the test gave it
    /// one.
    secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
    /// The scratch sessions cache /clear backs up into and allocates
    /// the next conversation's file under, kept alive for the test so
    /// nothing touches the machine's real cache.
    sessions: tempfile::TempDir,
}

/// Stores `key` as `provider`'s instance on `profile`, the login
/// flow's write shape.
fn store_key(profile: &mut rhema_config::Profile, provider: &str, key: &str) {
    profile.upsert_instance(
        provider,
        rhema_config::ProviderInstance {
            provider: provider.to_owned(),
            api_key: Some(key.to_owned()),
            secret_ref: None,
            base_url: None,
            api: None,
        },
    );
}

/// A profile serving turns on `provider` through one stored instance
/// named after it.
fn keyed_profile(provider: &str, key: &str) -> rhema_config::Profile {
    let mut profile = rhema_config::Profile {
        provider: Some(provider.to_owned()),
        ..rhema_config::Profile::default()
    };
    store_key(&mut profile, provider, key);
    profile
}

/// A test stand-in for a real approval hook: the parking half of the
/// bridge, sends the request event, blocks on the answer, declines on
/// any failure, without the production type nothing wires today.
struct ScriptedApproval {
    events: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    answers: std::sync::Mutex<std::sync::mpsc::Receiver<bool>>,
}

impl ScriptedApproval {
    /// The hook and its answerer over one std channel.
    fn pair(
        events: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    ) -> (Self, crate::approval::ApprovalAnswerer) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Self {
                events,
                answers: std::sync::Mutex::new(rx),
            },
            crate::approval::ApprovalAnswerer::sender(tx),
        )
    }
}

impl Approval for ScriptedApproval {
    fn approve(&self, call: &ToolCall) -> bool {
        if self
            .events
            .send(UiEvent::ApprovalRequested { call: call.clone() })
            .is_err()
        {
            return false;
        }
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv()
            == Ok(true)
    }
}

/// The Models page's picker: the list `/model` and Ctrl-P open.
fn models_page(h: &Harness) -> &ModelPicker {
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Models, "the Models page");
    &settings.model_picker
}

/// Tabs the sidebar to `section`, however many pages sit in between.
fn tab_to(h: &mut Harness, section: SettingsSection) {
    for _ in 0..SettingsSection::ALL.len() {
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        if settings.section == section {
            return;
        }
        h.app
            .on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    }
    panic!("the sidebar never reached {section:?}");
}

fn harness(api_key: Option<&str>, model: Option<&str>, session: Session) -> Harness {
    harness_inner(api_key, model, session, None)
}

/// A harness whose app places logins in a volatile store.
fn harness_with_secrets(api_key: Option<&str>, model: Option<&str>, session: Session) -> Harness {
    harness_inner(
        api_key,
        model,
        session,
        Some(Arc::new(rhema_config::secrets::VolatileSecretStore::new())),
    )
}

fn harness_inner(
    api_key: Option<&str>,
    model: Option<&str>,
    session: Session,
    secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
) -> Harness {
    let mut personal = rhema_config::Profile {
        // The key alone never served a turn: a usable
        // profile names its provider too.
        provider: Some("google".into()),
        model: model.map(str::to_owned),
        ..rhema_config::Profile::default()
    };
    if let Some(key) = api_key {
        personal.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: Some(key.to_owned()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
    }
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([("personal".to_owned(), personal)]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    let store = Arc::new(MemoryStore::new(config.clone()));
    let backend: Option<Backend> = api_key.map(|key| Backend::Google {
        api_key: key.to_owned(),
    });
    let sessions = tempfile::tempdir().unwrap();
    let app = App::new(AppSetup {
        store: store.clone(),
        checkpoint: None,
        sessions_base: Some(sessions.path().to_path_buf()),
        config,
        backend,
        session,
        secrets: secrets
            .clone()
            .map(|store| store as Arc<dyn rhema_config::secrets::SecretStore>),
        sandboxed: None,
        sandbox_machines: crate::sandbox::SandboxMachines::default(),
        home: Some(std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id()))),
        onboard: false,
        options: RunOptions {
            colors: crate::palette::Palette::Rhema,
            theme: crate::palette::ThemeKind::Dark,
            background: crate::palette::Background::Rhema,
            m3_source: None,
            custom: None,
            thinking: rhema_core::request::ThinkingEffort::Default,
            icons: crate::icons::Flavor::Unicode,
            cursor: crate::cursor::CursorStyle::Default,
            scrollbar: true,
            profile: Some("personal".into()),
            workspace: PathBuf::from("."),
            max_steps: NonZeroU32::new(4).unwrap(),
            no_transcript: false,
            base_system: None,
            save: None,
        },
    })
    .unwrap();
    Harness {
        app,
        store,
        secrets,
        sessions,
    }
}

/// Drives the settings' add-instance dialog to the credential stage
/// for `provider`, accepting the suggested instance name, ready to
/// type the credential. The settings must be open (or `/login`
/// dispatchable) first.
fn descend_to_credential(h: &mut Harness, provider: LoginProvider) -> String {
    use crate::screens::DialogStage;
    if !matches!(h.app.screen, Screen::Settings(_)) {
        crate::commands::dispatch(&mut h.app, "/login");
    }
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    // Down to the add row of the instance list, then enter.
    for _ in 0..settings.instance_rows().len().saturating_sub(1) {
        h.app.on_key(key(KeyCode::Down));
    }
    h.app.on_key(key(KeyCode::Enter));
    // The dialog opens on the categories: walk to the provider's group,
    // then to its row inside the group.
    let category = provider.category();
    let category_row = ProviderCategory::ALL
        .iter()
        .position(|entry| *entry == category)
        .expect("the provider's category is in the list");
    let provider_row = providers_in(category)
        .iter()
        .position(|entry| entry.kind == provider)
        .expect("the provider is in its category");
    for _ in 0..category_row {
        h.app.on_key(key(KeyCode::Down));
    }
    h.app.on_key(key(KeyCode::Enter));
    for _ in 0..provider_row {
        h.app.on_key(key(KeyCode::Down));
    }
    h.app.on_key(key(KeyCode::Enter));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    let dialog = settings.dialog.as_ref().expect("the add dialog");
    let name = dialog.input.clone();
    assert_eq!(dialog.stage, DialogStage::NameInstance);
    // Enter accepts the suggested name; copilot's flow starts
    // instead of asking for a key, which its own tests drive.
    h.app.on_key(key(KeyCode::Enter));
    name
}

#[test]
fn a_paste_on_the_add_dialog_fills_the_credential() {
    // The chat is the opening screen now; /login opens the page
    // the add dialog lives on.
    let mut h = harness(None, None, Session::default());
    assert!(matches!(h.app.screen, Screen::Chat));
    // Descend into the credential stage first.
    descend_to_credential(&mut h, LoginProvider::Google);
    h.app.on_paste("sk-123");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.dialog.as_ref().unwrap().input, "sk-123");
}

#[test]
fn no_resolvable_key_opens_the_chat() {
    // Nothing gates the session anymore: a keyless config is a
    // chat whose footer says "no provider" in red; the wizard is
    // for the first run only.
    let mut h = harness(None, None, Session::default());
    assert!(matches!(h.app.screen, Screen::Chat));
    // No key, no model: the only startup fetch is the version check.
    assert_eq!(h.app.initial_effects(), vec![Effect::CheckForUpdates]);
}

/// A store that refuses everything: the unreadable-store shape the
/// reachability probe exists to catch.
struct LockedStore;

impl rhema_config::secrets::SecretStore for LockedStore {
    fn reachable(&self) -> bool {
        false
    }

    fn get(&self, _id: &str) -> Result<Option<String>, rhema_config::secrets::SecretError> {
        Err(rhema_config::secrets::SecretError::NoStore)
    }

    fn set(&self, _id: &str, _value: &str) -> Result<(), rhema_config::secrets::SecretError> {
        Err(rhema_config::secrets::SecretError::NoStore)
    }

    fn delete(&self, _id: &str) -> Result<bool, rhema_config::secrets::SecretError> {
        Err(rhema_config::secrets::SecretError::NoStore)
    }
}

#[test]
fn an_unreadable_store_is_named_by_the_missing_key_help() {
    let profile = rhema_config::Profile {
        provider: Some("google".into()),
        ..rhema_config::Profile::default()
    };
    let err = Backend::from_profile_or_help(&profile, Some(&LockedStore)).unwrap_err();
    assert!(err.contains("GEMINI_API_KEY"), "{err}");
    assert!(err.contains("secret store is unavailable"), "{err}");
}

#[test]
fn the_plain_missing_key_help_names_no_store() {
    let profile = rhema_config::Profile {
        provider: Some("google".into()),
        ..rhema_config::Profile::default()
    };
    let err = Backend::from_profile_or_help(&profile, None).unwrap_err();
    assert!(err.contains("GEMINI_API_KEY"), "{err}");
    assert!(!err.contains("secret store"), "{err}");
}

#[test]
fn a_key_without_a_stored_model_opens_the_chat_quietly() {
    // No model stored is a red footer and /model one keystroke
    // away, not a gate: no fetch runs at startup and nothing but
    // the wizard stands between the user and the input box.
    let mut h = harness(Some("k"), None, Session::default());
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(h.app.agent.is_none());
    // The catalog still warms: ctrl-p answers from the cache the
    // moment the user asks for a model.
    assert_eq!(
        h.app.initial_effects(),
        vec![
            Effect::CheckForUpdates,
            Effect::FetchModels(FetchWhy::Bootstrap),
            Effect::FetchUsage
        ]
    );
}

#[test]
fn a_key_with_a_stored_model_opens_the_chat() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(h.app.agent.is_some());
    assert_eq!(
        h.app.initial_effects(),
        vec![
            Effect::CheckForUpdates,
            Effect::FetchModels(FetchWhy::Bootstrap),
            Effect::FetchUsage
        ]
    );
}

/// A landed allowance rides with the backend that fetched it; a
/// failed fetch clears the badge rather than leaving a stale
/// figure that reads as truth.
#[test]
fn the_allowance_lands_and_a_failed_fetch_clears_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::UsageLoaded {
        provider: "google",
        result: Ok(Some(rhema_provider::Allowance {
            amount: "$4.12".to_owned(),
            kind: String::new(),
        })),
    });
    let (provider, allowance) = h.app.subscription.as_ref().expect("allowance landed");
    assert_eq!(*provider, "google");
    assert_eq!(allowance.as_ref().expect("present").amount, "$4.12");

    h.app.on_event(UiEvent::UsageLoaded {
        provider: "google",
        result: Err("network error".to_owned()),
    });
    assert!(h.app.subscription.is_none());
}

#[test]
fn raced_nudges_chain_one_turn_per_finish_in_order() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (steer, rx) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
    for text in ["first", "second", "third"] {
        steer.blocking_send(text.into()).expect("queued");
    }
    drop(steer);
    let agent = Box::new(h.app.agent.take().unwrap());
    let effects = h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: rx,
    });
    // Exactly one turn leaves per finish; the rest wait their turn.
    // (The finish also refreshes usage; that rides along.)
    assert_eq!(
        effects,
        vec![
            Effect::StartTurn {
                spans: Vec::new(),
                text: "first".into(),
                images: Vec::new()
            },
            Effect::FetchUsage,
        ]
    );
    assert_eq!(
        Vec::from_iter(h.app.pending_prompts.iter()),
        ["second", "third"]
    );

    // The chained turn's finish sends the next one out.
    let agent = Box::new(h.app.agent.take().unwrap());
    let effects = h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert_eq!(
        effects,
        vec![
            Effect::StartTurn {
                spans: Vec::new(),
                text: "second".into(),
                images: Vec::new()
            },
            Effect::FetchUsage,
        ]
    );
    assert_eq!(Vec::from_iter(h.app.pending_prompts.iter()), ["third"]);

    // A cancel spends no queued words: they return to the input.
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnCancelled {
        agent,
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(h.app.pending_prompts.is_empty());
    assert_eq!(h.app.chat.input, "third");
}

/// The turn just moved the account's counters: finishing (or
/// cancelling) one schedules the allowance refresh.
#[test]
fn a_finished_turn_refreshes_the_allowance() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let agent = Box::new(h.app.agent.take().unwrap());
    let effects = h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert_eq!(effects, vec![Effect::FetchUsage]);
}

/// A reasoning-only response, a content-less assistant message the
/// provider ended itself, cannot pass for an answer: the transcript
/// names what happened. A reply that said something raises nothing.
#[test]
fn an_empty_turn_surfaces_its_stop_reason() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![],
            },
            stop_reason: StopReason::MaxTokens,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(h.app.transcript.lines().contains(&Line::Notice(
        "the model returned no content (stopped: max tokens)".into()
    )));

    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    // The first turn's notice rightly persists; a real reply raised
    // no second one.
    assert_eq!(
        h.app
            .transcript
            .lines()
            .iter()
            .filter(|line| matches!(
                line,
                Line::Notice(text) if text.contains("no content")
            ))
            .count(),
        1
    );
}

/// A model switch lands a new backend: its allowance fetches at
/// once, not at the first turn's end.
#[test]
fn a_model_switch_refreshes_the_allowance() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let effects = h.app.choose_model("google", Some("google"), "m-a");
    assert_eq!(effects, vec![Effect::FetchUsage]);
}

#[test]
fn login_persists_the_key_without_opening_a_picker() {
    let mut h = harness(None, None, Session::default());
    descend_to_credential(&mut h, LoginProvider::Google);
    for c in "secret".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(
        matches!(h.app.transcript.lines().last(), Some(Line::Notice(text)) if text.contains("added Google Gemini") && text.contains("/model"))
    );
    assert!(
        h.app.catalog_stale,
        "the picker must refresh before it lists the new provider"
    );
    let effective = h.store.load().unwrap().effective(None);
    assert_eq!(
        effective.instances["Google Gemini"].api_key.as_deref(),
        Some("secret")
    );
    assert!(effective.model.is_none(), "no model is chosen at add time");
    // The profile already ran on google: adding a second instance of
    // the same provider does not silently switch the active one.
    assert_eq!(effective.instance, None);
    assert!(h.app.backend.is_none());
    assert!(h.app.agent.is_none());

    // /model is where selection happens; it fetches because the cache
    // is empty and the add marked it stale.
    let effects = crate::commands::dispatch(&mut h.app, "/model");
    assert_eq!(effects, vec![Effect::FetchModels(FetchWhy::Refresh)]);
    assert!(
        matches!(&h.app.screen, Screen::Settings(settings) if settings.section == SettingsSection::Models)
    );
    assert!(!h.app.catalog_stale);
}

#[test]
fn a_login_places_its_key_in_the_store_and_the_file_stays_clean() {
    let mut h = harness_with_secrets(None, None, Session::default());
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    descend_to_credential(&mut h, LoginProvider::Google);
    for c in "secret".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    let effective = h.store.load().unwrap().effective(None);
    let instance = &effective.instances["Google Gemini"];
    assert_eq!(instance.api_key, None, "the file holds no key");
    let id = instance.secret_ref.as_deref().expect("the store ref");
    assert_eq!(secrets.get(id).unwrap().as_deref(), Some("secret"));
    // The session's own resolution reads the chain: the login
    // serves turns with no literal anywhere.
    assert_eq!(
        h.app
            .effective()
            .api_key("google", h.app.secret_store())
            .as_deref(),
        Some("secret")
    );
}

#[test]
fn removing_an_instance_deletes_its_store_entry() {
    let mut h = harness_with_secrets(Some("g"), Some("m"), Session::default());
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    // The profile's one instance logs in through the chain: the
    // ref is live, the literal is gone.
    secrets.set("ref-1", "g").unwrap();
    h.app.config.set_for_profile(Some("personal"), |p| {
        let instance = p.instances.get_mut("google").unwrap();
        instance.secret_ref = Some("ref-1".into());
        instance.api_key = None;
    });
    // /login opens the providers page over the session's profile;
    // the instance row is selected, `d` arms the confirm, `y` forgets.
    for c in "/login".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Settings(_)));
    let _ = h.app.on_key(key(KeyCode::Char('d')));
    let _ = h.app.on_key(key(KeyCode::Char('y')));
    let effective = h.store.load().unwrap().effective(None);
    assert!(!effective.instances.contains_key("google"), "forgotten");
    assert_eq!(secrets.get("ref-1").unwrap(), None, "the entry went too");
}

#[test]
fn an_add_keeps_the_catalog_and_the_next_picker_refresh_merges_it() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    // /login, then the openrouter row and a key.
    for c in "/login".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    descend_to_credential(&mut h, LoginProvider::OpenRouter);
    for c in "o".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![], "adding a provider fetches no catalog");
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(h.app.models.len(), 2, "the google cache is untouched");
    assert!(h.app.catalog_stale);

    // /model asks for every provider's catalog: the fresh slices merge
    // instead of replacing the cache.
    let effects = crate::commands::dispatch(&mut h.app, "/model");
    assert_eq!(effects, vec![Effect::FetchModels(FetchWhy::Refresh)]);
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: [
            loaded("google", models()),
            loaded(
                "openrouter",
                vec![llama_model("openrouter/auto"), llama_model("z-ai/glm-5.3")],
            ),
        ]
        .concat(),
    });
    let picker = models_page(&h);
    assert_eq!(picker.models.len(), 4, "google stays: {picker:?}");
    let providers: Vec<&str> = picker.models.iter().map(|m| m.provider).collect();
    assert_eq!(providers, ["google", "google", "openrouter", "openrouter"]);
}

#[test]
fn an_add_before_a_late_bootstrap_keeps_both_catalogs() {
    // The startup fetch races an add: it was spawned first, but a slow
    // catalog can land it afterwards. Its slices replace themselves;
    // the add leaves the cache alone and marks the next picker stale.
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    for c in "/login".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    descend_to_credential(&mut h, LoginProvider::OpenRouter);
    for c in "o".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert_eq!(h.app.models.len(), 2);

    // The late startup answer covers google only; it cannot blank the
    // cache or the openrouter instance's place in the profile.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    let providers: Vec<&str> = h.app.models.iter().map(|m| m.provider).collect();
    assert_eq!(providers, ["google", "google"]);
    assert!(
        h.store
            .load()
            .unwrap()
            .effective(None)
            .instances
            .contains_key("OpenRouter")
    );
}

#[test]
fn llama_login_persists_the_provider_without_a_model_probe() {
    let mut h = harness(None, None, Session::default());
    // The llama.cpp row, the suggested name, the URL, and a key.
    let name = descend_to_credential(&mut h, LoginProvider::LlamaCpp);
    assert_eq!(name, "llama.cpp");
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(
                login.pending.as_ref().map(|p| p.provider),
                Some(crate::screens::LoginProvider::LlamaCpp)
            );
            assert_eq!(login.input_label(), "server URL: ");
        }
        _ => panic!("expected the login screen"),
    }
    for c in "http://box:8080".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    // Enter captures the URL; the bearer key prompt follows, and a
    // blank one is accepted (an open server).
    h.app.on_key(key(KeyCode::Enter));
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(login.input_label(), "API key: ");
        }
        _ => panic!("expected the bearer-key prompt"),
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![], "adding a provider does not fetch models");
    assert!(matches!(h.app.screen, Screen::Chat));
    let config = h.store.load().unwrap();
    let effective = config.effective(None);
    assert_eq!(
        effective.instances["llama.cpp"].base_url.as_deref(),
        Some("http://box:8080")
    );
    assert!(effective.model.is_none());
    assert!(
        h.app.backend.is_none(),
        "the active google profile stays put"
    );
}

#[test]
fn anthropic_login_persists_the_key_without_a_model_probe() {
    let mut h = harness(None, None, Session::default());
    // Anthropic's row, the suggested name, and the key.
    let name = descend_to_credential(&mut h, LoginProvider::Anthropic);
    assert_eq!(name, "Anthropic");
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(login.input_label(), "API key: ");
        }
        _ => panic!("expected the login screen"),
    }
    for c in "sk-ant-01".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    let config = h.store.load().unwrap();
    let effective = config.effective(None);
    assert_eq!(
        effective.instances["Anthropic"].api_key.as_deref(),
        Some("sk-ant-01")
    );
    assert!(effective.model.is_none());
}

#[test]
fn openai_login_persists_the_key_with_the_default_base() {
    let mut h = harness(None, None, Session::default());
    // OpenAI's row, the suggested name, and the key. Hosted providers
    // go straight to the key; only a custom endpoint asks for a URL.
    descend_to_credential(&mut h, LoginProvider::OpenAi);
    for c in "sk-proj-01".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    let stored = h.store.load().unwrap();
    let effective = stored.effective(None);
    assert_eq!(
        effective.instances["OpenAI"].api_key.as_deref(),
        Some("sk-proj-01")
    );
    assert!(effective.model.is_none());
}

#[test]
fn zai_login_persists_the_key_with_the_default_route() {
    let mut h = harness(None, None, Session::default());
    // GLM Coding Plan's row, the suggested name, and the key. The
    // plan's own route is fixed, so no URL prompt.
    descend_to_credential(&mut h, LoginProvider::Zai);
    for c in "zkey-01".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    let stored = h.store.load().unwrap();
    let effective = stored.effective(None);
    assert_eq!(
        effective.instances["GLM Coding Plan"].api_key.as_deref(),
        Some("zkey-01")
    );
    assert!(effective.model.is_none());
}

#[test]
fn a_zai_profile_builds_its_own_backend_from_a_stored_route() {
    // A stored instance with a base URL override: the backend points
    // the adapter at that route instead of the Coding Plan's.
    let mut profile = keyed_profile("zai", "k-z");
    profile.instance = Some("zai".into());
    profile.upsert_instance(
        "proxy",
        rhema_config::ProviderInstance {
            provider: "zai".into(),
            api_key: Some("k-p".into()),
            secret_ref: None,
            base_url: Some("https://proxy.example/api/anthropic".into()),
            api: None,
        },
    );
    let backend = Backend::from_profile(&profile, None).expect("a keyed zai profile serves turns");
    assert_eq!(backend.config_name(), "zai");
    assert!(
        matches!(
            backend,
            Backend::Zai { ref api_key, base_url: None }
                if api_key == "k-z"
        ),
        "the active zai instance serves: {backend:?}"
    );
    // Activating the proxy instance moves both the key and the route.
    profile.instance = Some("proxy".into());
    assert!(
        matches!(
            Backend::from_profile(&profile, None),
            Some(Backend::Zai { ref api_key, base_url: Some(ref url) })
                if api_key == "k-p" && url == "https://proxy.example/api/anthropic"
        ),
        "the proxy instance's route serves"
    );
    // No key, no backend, the kind is not served by another's key.
    let mut cross = rhema_config::Profile {
        provider: Some("zai".into()),
        ..rhema_config::Profile::default()
    };
    store_key(&mut cross, "anthropic", "k-a");
    assert_eq!(
        Backend::from_profile(&cross, None),
        None,
        "a zai profile is not served by an anthropic key"
    );
}

#[test]
fn a_cross_provider_backend_keeps_the_stored_route() {
    let mut profile = keyed_profile("anthropic", "k-a");
    profile.upsert_instance(
        "local-openai",
        rhema_config::ProviderInstance {
            provider: "openai".into(),
            api_key: Some("k-o".into()),
            secret_ref: None,
            base_url: Some("http://localhost:8000/v1".into()),
            api: Some(rhema_config::ProviderApi::Chat),
        },
    );

    let backend = Backend::from_kind("openai", &profile, None)
        .expect("the stored OpenAI instance provides a key");
    assert_eq!(backend.config_name(), "openai");
    assert!(
        matches!(
            backend,
            Backend::OpenAi { ref api_key, ref base_url, api: Some(rhema_config::ProviderApi::Chat) }
                if api_key == "k-o"
                    && base_url.as_deref() == Some("http://localhost:8000/v1")
        ),
        "a subagent's cross-provider backend keeps the instance route"
    );
}

#[test]
fn backend_debug_redacts_every_credential() {
    let backend = Backend::Anthropic {
        api_key: "sk-do-not-print".into(),
    };
    let debug = format!("{backend:?}");
    assert!(debug.contains("Anthropic"));
    assert!(!debug.contains("sk-do-not-print"), "{debug}");
    assert!(debug.contains("redacted"), "{debug}");

    let backend = Backend::ChatGpt {
        credential: "oauth-json".into(),
    };
    let debug = format!("{backend:?}");
    assert!(!debug.contains("oauth-json"), "{debug}");
}

#[test]
fn an_opencode_profile_keeps_its_stored_go_route() {
    let mut profile = keyed_profile("opencode", "k-oc");
    profile.instance = Some("OpenCode Go".into());
    profile.upsert_instance(
        "OpenCode Go",
        rhema_config::ProviderInstance {
            provider: "opencode".into(),
            api_key: Some("k-go".into()),
            secret_ref: None,
            base_url: Some("https://opencode.ai/inference/go/openai/v1".into()),
            api: None,
        },
    );
    let backend =
        Backend::from_profile(&profile, None).expect("a keyed OpenCode profile serves turns");
    assert_eq!(backend.config_name(), "opencode");
    assert!(
        matches!(
            backend,
            Backend::OpenCode { ref api_key, ref base_url, .. }
                if api_key == "k-go"
                    && base_url.as_deref() == Some("https://opencode.ai/inference/go/openai/v1")
        ),
        "the active OpenCode instance's Go route serves"
    );
}

#[test]
fn anthropic_and_openai_profiles_build_their_own_backends() {
    let keyed = |provider: &str, key: Option<&str>| {
        let mut profile = rhema_config::Profile {
            provider: Some(provider.to_owned()),
            ..rhema_config::Profile::default()
        };
        if let Some(key) = key {
            store_key(&mut profile, provider, key);
        }
        profile
    };
    let backend = Backend::from_profile(&keyed("anthropic", Some("k-a")), None)
        .expect("a keyed anthropic profile serves turns");
    assert_eq!(backend.config_name(), "anthropic");
    assert!(
        matches!(backend, Backend::Anthropic { ref api_key } if api_key == "k-a"),
        "the anthropic profile builds the anthropic backend: {backend:?}"
    );
    let backend = Backend::from_profile(&keyed("openai", Some("k-o")), None)
        .expect("a keyed openai profile serves turns");
    assert_eq!(backend.config_name(), "openai");
    assert!(
        matches!(backend, Backend::OpenAi { ref api_key, .. } if api_key == "k-o"),
        "the openai profile builds the openai backend: {backend:?}"
    );
    // No key, no backend, a keyless profile cannot serve turns.
    let mut cross = rhema_config::Profile {
        provider: Some("anthropic".into()),
        ..rhema_config::Profile::default()
    };
    store_key(&mut cross, "openai", "k-o");
    assert_eq!(
        Backend::from_profile(&cross, None),
        None,
        "an anthropic profile is not served by an openai key"
    );
    assert_eq!(
        Backend::from_profile(
            &rhema_config::Profile {
                provider: Some("openai".into()),
                ..rhema_config::Profile::default()
            },
            None,
        ),
        None
    );
    // No provider named, and a spelling no provider answers to, both
    // leave the backend unresolved: the login flow runs, nothing
    // silently serves google.
    assert_eq!(
        Backend::from_profile(&rhema_config::Profile::default(), None),
        None,
        "a provider-less profile opens the login flow"
    );
    let mut nonsense = rhema_config::Profile {
        provider: Some("nonsense".into()),
        ..rhema_config::Profile::default()
    };
    store_key(&mut nonsense, "google", "k");
    assert_eq!(
        Backend::from_profile(&nonsense, None),
        None,
        "an unknown spelling never falls back to google"
    );
}

#[test]
fn candidates_span_every_provider_the_profile_holds_keys_for() {
    let mut profile = keyed_profile("openrouter", "o");
    store_key(&mut profile, "google", "g");
    let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
        .iter()
        .map(|candidate| candidate.backend.config_name())
        .collect();
    assert_eq!(names, ["openrouter", "google"], "the active provider leads");

    // llama.cpp joins only as the active provider: a server address
    // is a login, not a catalog to browse.
    profile.provider = Some("llama-cpp".into());
    let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
        .iter()
        .map(|candidate| candidate.backend.config_name())
        .collect();
    assert_eq!(names, ["llama-cpp", "google", "openrouter"]);
}

#[test]
fn candidates_name_every_stored_instance_and_put_the_active_one_first() {
    let profile = rhema_config::Profile {
        provider: Some("llama-cpp".to_owned()),
        instance: Some("gpu box".to_owned()),
        instances: BTreeMap::from([
            (
                "gpu box".to_owned(),
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://gpu:8080".into()),
                    api: None,
                },
            ),
            (
                "home".to_owned(),
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                    api: None,
                },
            ),
            (
                "work sub".to_owned(),
                rhema_config::ProviderInstance {
                    provider: "copilot".into(),
                    api_key: Some("gho-1".into()),
                    secret_ref: None,
                    base_url: None,
                    api: None,
                },
            ),
        ]),
        ..rhema_config::Profile::default()
    };
    let candidates = Backend::candidates_from_profile(&profile, None);
    let rows: Vec<(Option<&str>, &str)> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.instance.as_deref(),
                candidate.backend.config_name(),
            )
        })
        .collect();
    // The active instance leads, the rest follow in name order,
    // every llama.cpp server joins, a stored URL being an explicit
    // login now.
    assert_eq!(
        rows,
        [
            (Some("gpu box"), "llama-cpp"),
            (Some("home"), "llama-cpp"),
            (Some("work sub"), "copilot"),
        ]
    );
    assert!(
        matches!(
            candidates[0].backend,
            Backend::LlamaCpp { ref url, .. } if url == "http://gpu:8080"
        ),
        "the active instance's own URL serves: {:?}",
        candidates[0].backend
    );
}

#[test]
fn refresh_merges_every_providers_catalog_and_names_the_failures() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: [
            loaded("google", models()),
            loaded("openrouter", vec![llama_model("openrouter/auto")]),
        ]
        .concat(),
    });
    h.app.on_key(ctrl('p'));
    // A refresh where google's catalog fails: the openrouter one keeps
    // the picker alive, and the footer names the gap.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: [
            loaded("openrouter", vec![llama_model("openrouter/auto")]),
            failed("google", "gemini api error (HTTP 403): denied"),
        ]
        .concat(),
    });
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Ready);
    assert_eq!(picker.failed, ["google"]);
    assert_eq!(picker.models.len(), 1);
    assert_eq!(picker.models[0].provider, "openrouter");
}

#[test]
fn a_bootstrap_timeout_is_kept_for_the_next_picker() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    // The bootstrap lands on the chat: google answers, the LAN server
    // does not. The picker opens later and still names the dead one.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: [loaded("google", models()), unavailable("llama-cpp")].concat(),
    });
    assert_eq!(h.app.catalog_unavailable, ["llama-cpp"]);
    h.app.on_key(ctrl('p'));
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Ready);
    assert_eq!(picker.unavailable, ["llama-cpp"]);
    assert!(picker.failed.is_empty());
}

#[test]
fn a_provider_that_answers_again_leaves_the_unavailable_list() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: unavailable("llama-cpp"),
    });
    h.app.on_key(ctrl('p'));
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: loaded("llama-cpp", vec![llama_model("local/model")]),
    });
    assert!(h.app.catalog_unavailable.is_empty());
    assert!(models_page(&h).unavailable.is_empty());
}

#[test]
fn a_total_catalog_failure_is_the_pickers_error() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    h.app.on_key(ctrl('p'));
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: failed("google", "gemini api error (HTTP 403): denied"),
    });
    assert!(matches!(
        models_page(&h).status,
        ModelPickerStatus::Error(_)
    ));
}

#[test]
fn slash_model_with_a_needle_switches_directly() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    for c in "/model google/m-b".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::FetchUsage],
        "a named model rebuilds inline"
    );
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-b")
    );
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("model switched to google/m-b".into()))
    );
}

#[test]
fn slash_model_with_a_unique_prefix_resolves_and_an_unknown_needle_says_so() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    for c in "/model google/m-".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(h.app.screen, Screen::Chat),
        "the picker never opens"
    );
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("2 models match"))),
        "ambiguous prefix says so"
    );
    for c in "/model nope".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("no model matches")))
    );
    // A bare id names no provider, so it resolves to nothing: the
    // needle must be the fully-qualified row.
    for c in "/model m-b".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("no model matches"))),
        "a bare id is declined"
    );
}

#[test]
fn slash_switch_changes_the_model_for_the_session_only() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    let effects = crate::commands::dispatch(&mut h.app, "/switch google/m-b");
    assert_eq!(effects, vec![Effect::FetchUsage], "the rebuild is inline");
    assert!(matches!(h.app.screen, Screen::Chat));
    // The session runs on the picked model, the config keeps the
    // stored one.
    assert_eq!(h.app.effective().model.as_deref(), Some("m-b"));
    assert_eq!(
        h.app.session_model.as_ref().map(|pick| pick.model.as_str()),
        Some("m-b")
    );
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-a"),
        "the pick never reaches the store"
    );
    assert!(h.app.transcript.lines().contains(&Line::Notice(
        "model switched to google/m-b for this session; the profile keeps its stored model".into()
    )));
}

#[test]
fn slash_switch_without_a_needle_opens_the_picker() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/switch");
    match &h.app.screen {
        Screen::Picker(picker) => {
            assert!(!picker.models.is_empty(), "the cached catalog lists");
        }
        _ => panic!("expected the switch picker"),
    }
}

/// The `/switch` picker's Enter is the session-only half of a pick: the
/// backend rebuilds, the profile keeps its stored model.
#[test]
fn the_switch_picker_picks_without_saving() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/switch");
    for c in "m-b".chars() {
        h.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
    }
    let effects = h
        .app
        .on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(effects, vec![Effect::FetchUsage], "the rebuild is inline");
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(h.app.effective().model.as_deref(), Some("m-b"));
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-a"),
        "the pick never reaches the store"
    );
}

#[test]
fn a_persisted_model_pick_replaces_the_session_only_one() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/switch google/m-b");
    assert_eq!(h.app.effective().model.as_deref(), Some("m-b"));
    crate::commands::dispatch(&mut h.app, "/model google/m-a");
    assert!(h.app.session_model.is_none(), "the stored pick wins");
    assert_eq!(h.app.effective().model.as_deref(), Some("m-a"));
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-a")
    );
}

#[test]
fn a_profile_switch_drops_the_session_only_model() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/switch google/m-b");
    assert_eq!(h.app.effective().model.as_deref(), Some("m-b"));
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "work-key"));
    crate::commands::dispatch(&mut h.app, "/profile work");
    assert!(h.app.session_model.is_none());
    assert_eq!(h.app.effective().model, None, "work stores no model");
}

#[test]
fn a_resumed_session_continues_its_cost() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    session
        .history
        .push(Message::text(Role::User, "earlier work"));
    session.cost_usd = 2.5;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    crate::sessions::write_session(&path, &session).unwrap();

    h.app.resume_session(&path);
    assert_eq!(h.app.session_cost, 2.5, "the footer continues the story");
    // A turn on top of it accumulates further, and the exit save
    // carries the total back into the file.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    h.app.config.set_for_profile(Some("personal"), |settings| {
        settings.model = Some("m-a".into())
    });
    let (exported, origin) = h.app.export_session().unwrap();
    assert_eq!(origin.as_deref(), Some(path.as_path()));
    assert_eq!(exported.cost_usd, 2.5);
}

#[test]
fn a_resume_pick_under_a_running_turn_is_declined() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    assert!(h.app.request_resume(Path::new("/tmp/none.json")).is_empty());
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn a_failed_resume_load_lands_in_the_error_strip() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let path = Path::new("/tmp/rhema-test-no-such-session.json");
    assert_eq!(
        h.app.request_resume(path),
        vec![Effect::ResumeSession(path.to_path_buf())]
    );
    h.app.on_event(UiEvent::SessionResumed {
        path: path.to_path_buf(),
        result: Err("no such file".into()),
    });
    assert_eq!(
        h.app.error.as_deref(),
        Some("cannot load session: no such file")
    );
    // The screen held through the failure: no half-applied session,
    // the harness's own agent is still the live one.
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn a_resumed_session_shows_its_saved_usage() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    session
        .history
        .push(Message::text(Role::User, "earlier work"));
    let saved_stats = rhema_transcript::TranscriptStats {
        requests: 7,
        input_tokens: 90_000,
        cached_input_tokens: 70_000,
        output_tokens: 1_000,
        usage: rhema_transcript::UsageBreakdown {
            system_prompt_tokens: 20_000,
            ..rhema_transcript::UsageBreakdown::default()
        },
        ..rhema_transcript::TranscriptStats::default()
    };
    let last_turn = Usage {
        input_tokens: 3_000,
        output_tokens: 300,
        cached_input_tokens: 2_000,
    };
    crate::sessions::UsageStats {
        transcript: Some(saved_stats),
        last_turn: Some(last_turn),
        transcript_sections: None,
    }
    .write_into(&mut session);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    crate::sessions::write_session(&path, &session).unwrap();

    h.app.resume_session(&path);
    // The gauge picks up the last turn, the mirror the counters.
    assert_eq!(h.app.usage, Some(last_turn));
    assert_eq!(
        h.app.agent.as_ref().unwrap().transcript_stats(),
        Some(saved_stats)
    );
    // And /settings's Usage page reads the whole thing.
    h.app.open_settings_at(SettingsSection::Usage);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("settings should be open");
    };
    assert_eq!(settings.last_turn, Some(last_turn));
    assert_eq!(settings.transcript, Some(saved_stats));
    assert_eq!(settings.session_cost, 0.0);
}

#[test]
fn removing_a_websearch_provider_deletes_its_store_entry() {
    let mut h = harness_with_secrets(Some("k"), None, Session::default());
    h.secrets
        .as_ref()
        .unwrap()
        .set("ws-1", "brave-key")
        .unwrap();
    let personal = h
        .app
        .config
        .settings
        .profiles
        .get_mut("personal")
        .expect("the harness profile");
    personal.websearch.insert(
        "brave-main".to_owned(),
        rhema_config::WebSearchInstance {
            provider: "brave".into(),
            api_key: None,
            secret_ref: Some("ws-1".into()),
        },
    );
    personal.websearch_order.push("brave-main".to_owned());

    h.app.open_settings_at(SettingsSection::WebSearch);
    assert!(h.app.remove_websearch_provider().is_empty());
    assert!(h.app.effective().websearch.is_empty());
    assert_eq!(
        h.secrets.as_ref().unwrap().get("ws-1").unwrap(),
        None,
        "the removed provider leaves no key in the chain"
    );
}

#[test]
fn the_saved_usage_survives_a_save_and_resume_round_trip() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    session
        .history
        .push(Message::text(Role::User, "earlier work"));
    let saved_stats = rhema_transcript::TranscriptStats {
        requests: 7,
        input_tokens: 90_000,
        output_tokens: 1_000,
        ..rhema_transcript::TranscriptStats::default()
    };
    crate::sessions::UsageStats {
        transcript: Some(saved_stats),
        last_turn: None,
        transcript_sections: None,
    }
    .write_into(&mut session);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    crate::sessions::write_session(&path, &session).unwrap();

    // Resume, then save: the exit save re-stamps what the run
    // measured, the carried counters, nothing lost to the fresh
    // mirror starting at zero.
    h.app.resume_session(&path);
    let (exported, _) = h.app.export_session().unwrap();
    assert_eq!(
        crate::sessions::UsageStats::from_session(&exported)
            .unwrap()
            .transcript,
        Some(saved_stats)
    );

    // And what was saved, a second resume shows.
    crate::sessions::write_session(&path, &exported).unwrap();
    let mut second = harness(Some("k"), Some("m-a"), Session::default());
    second.app.resume_session(&path);
    assert_eq!(
        second.app.agent.unwrap().transcript_stats(),
        Some(saved_stats)
    );
}

#[test]
fn a_turn_on_a_resumed_session_extends_the_saved_counters() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    session
        .history
        .push(Message::text(Role::User, "earlier work"));
    let baseline = rhema_transcript::TranscriptStats {
        requests: 7,
        input_tokens: 90_000,
        output_tokens: 1_000,
        ..rhema_transcript::TranscriptStats::default()
    };
    crate::sessions::UsageStats {
        transcript: Some(baseline),
        last_turn: None,
        transcript_sections: None,
    }
    .write_into(&mut session);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.json");
    crate::sessions::write_session(&path, &session).unwrap();
    h.app.resume_session(&path);

    // A turn ran elsewhere; its own mirror counted one request.
    // Folding it at capture keeps the run's totals.
    let mut agent = h.app.agent.take().unwrap();
    agent.carry_transcript_stats(&rhema_transcript::TranscriptStats {
        requests: 2,
        input_tokens: 5_000,
        output_tokens: 400,
        ..rhema_transcript::TranscriptStats::default()
    });
    h.app.on_event(UiEvent::TurnFinished {
        agent: Box::new(agent),
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    let stats = h.app.transcript_stats.unwrap();
    assert_eq!(stats.requests, 9, "the run's requests, not the turn's");
    assert_eq!(stats.input_tokens, 95_000);
    assert_eq!(stats.output_tokens, 1_400);
}

#[test]
fn slash_clear_drops_the_usage_accounting() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let baseline = rhema_transcript::TranscriptStats {
        requests: 7,
        input_tokens: 90_000,
        ..rhema_transcript::TranscriptStats::default()
    };
    h.app
        .agent
        .as_mut()
        .unwrap()
        .carry_transcript_stats(&baseline);
    h.app.usage = Some(Usage::default());
    crate::commands::dispatch(&mut h.app, "/clear");
    assert_eq!(h.app.usage, None);
    assert_eq!(h.app.transcript_stats, None);
    // The mirror starts over too: the page cannot quote the
    // cleared conversation.
    assert_eq!(
        h.app.agent.unwrap().transcript_stats(),
        Some(rhema_transcript::TranscriptStats::default())
    );
}

#[test]
fn a_profile_switch_keeps_the_usage_counters() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        store_key(settings, "openrouter", "o");
    });
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "o"));
    let baseline = rhema_transcript::TranscriptStats {
        requests: 7,
        input_tokens: 90_000,
        ..rhema_transcript::TranscriptStats::default()
    };
    h.app
        .agent
        .as_mut()
        .unwrap()
        .carry_transcript_stats(&baseline);
    for c in "/profile work".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Chat));
    let stats = h.app.agent.unwrap().transcript_stats().unwrap();
    assert_eq!(stats.requests, 7, "the counters moved with the chat");
}

#[test]
fn slash_profile_switches_to_a_named_profile() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        store_key(settings, "openrouter", "o");
    });
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "o"));
    for c in "/profile work".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    assert!(matches!(
        h.app.backend,
        Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
    ));
}

#[test]
fn choosing_from_another_provider_switches_the_backend() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        store_key(settings, "openrouter", "o");
    });
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: [
            loaded("google", models()),
            loaded("openrouter", vec![llama_model("openrouter/auto")]),
        ]
        .concat(),
    });
    // Filter by provider, pick the openrouter model.
    h.app.on_key(ctrl('p'));
    for c in "openrouter".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::FetchUsage], "a pick rebuilds inline");
    assert!(matches!(
        &h.app.screen,
        Screen::Settings(settings) if settings.section == SettingsSection::Models
    ));
    assert!(matches!(
        h.app.backend,
        Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
    ));
    let effective = h.store.load().unwrap().effective(None);
    assert_eq!(effective.provider.as_deref(), Some("openrouter"));
    assert_eq!(effective.model.as_deref(), Some("openrouter/auto"));
    assert!(h.app.transcript.lines().contains(&Line::Notice(
        "model switched to openrouter/openrouter/auto".into()
    )));
}

#[test]
fn turn_costs_accumulate_from_the_model_pricing() {
    // No stored model: the chat opens bare and the test names the
    // priced model through /model, the move the red footer
    // invites, before the turn runs.
    let mut h = harness(
        Some("k"),
        None,
        Session {
            history: vec![Message::text(Role::User, "earlier work")],
            driver_state: serde_json::Value::Null,
            cost_usd: 0.0,
            usage_stats: serde_json::Value::Null,
        },
    );
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        // $0.75/M in, $3.75/M out, cached input at a quarter, the
        // shape the google adapter bakes for a flash model.
        result: loaded(
            "google",
            vec![priced_model(
                "gemini-3.8-flash",
                rhema_provider::Pricing {
                    input_per_mtok: 0.75,
                    output_per_mtok: 3.75,
                    cached_input_per_mtok: Some(0.1875),
                },
            )],
        ),
    });
    crate::commands::dispatch(&mut h.app, "/model google/gemini-3.8-flash");
    assert!(h.app.agent.is_some(), "the pick built the agent");
    h.app.on_key(key(KeyCode::Char('a')));
    let effects = h.app.on_key(key(KeyCode::Enter));
    match effects.as_slice() {
        [Effect::StartTurn { images, .. }] => {
            assert!(images.is_empty());
        }
        other => panic!("expected a turn start, got {other:?}"),
    }
    let agent = Box::new(h.app.agent.take().unwrap());
    // The turn's one model call lands as the engine's own StepEnd:
    // pricing rides the step, not the turn's final output.
    h.app.on_event(UiEvent::Engine(OwnedEvent::StepEnd {
        usage: Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cached_input_tokens: 400_000,
        },
        stop_reason: StopReason::EndTurn,
    }));
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cached_input_tokens: 400_000,
            },
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    // The priced StepEnd: 600k uncached in at $0.75/M + 400k cached in
    // at a quarter of that + 1M out at $3.75/M.
    assert!((h.app.session_cost - 4.275).abs() < 1e-9);
    // The exit save carries the total, so a resume continues it.
    let (exported, _) = h.app.export_session().unwrap();
    assert!((exported.cost_usd - 4.275).abs() < 1e-9);
}

#[test]
fn adding_a_single_model_server_still_waits_for_model_choice() {
    let mut h = harness(None, None, Session::default());
    // The llama.cpp row, the suggested name, the URL, and a blank
    // bearer key.
    descend_to_credential(&mut h, LoginProvider::LlamaCpp);
    for c in "http://localhost:8080".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    // Even a one-model server is not picked at add time; /model does
    // that, so the choice stays the user's.
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(h.app.agent.is_none());
    assert!(h.store.load().unwrap().effective(None).model.is_none());
}

#[test]
fn enter_on_a_stored_instance_activates_it() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        settings.provider = Some("llama-cpp".into());
        settings.instance = Some("home".into());
        settings.model = Some("qwen3-8b".into());
        settings.upsert_instance(
            "home",
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some("http://home:8080".into()),
                api: None,
            },
        );
        settings.upsert_instance(
            "gpu box",
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some("http://gpu:8080".into()),
                api: None,
            },
        );
    });
    crate::commands::dispatch(&mut h.app, "/login");
    // "gpu box" sorts before "home": the first row is it.
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::FetchUsage],
        "a stored model goes straight to chat"
    );
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(
        matches!(&h.app.backend, Some(Backend::LlamaCpp { url, .. }) if url == "http://gpu:8080"),
        "the activated instance's own URL serves: {:?}",
        h.app.backend
    );
    let effective = h.store.load().unwrap().effective(None);
    assert_eq!(effective.instance.as_deref(), Some("gpu box"));
    assert_eq!(
        effective.model.as_deref(),
        Some("qwen3-8b"),
        "the same provider keeps its stored model"
    );
}

#[test]
fn activating_across_providers_drops_the_foreign_model() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        settings.provider = Some("openai".into());
        settings.instance = Some("openai".into());
        settings.model = Some("gpt-5".into());
        settings.upsert_instance(
            "openai",
            rhema_config::ProviderInstance {
                provider: "openai".into(),
                api_key: Some("sk".into()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        settings.upsert_instance(
            "home",
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some("http://home:8080".into()),
                api: None,
            },
        );
    });
    crate::commands::dispatch(&mut h.app, "/login");
    // "home" sorts before "openai": the first row is it.
    let effects = h.app.on_key(key(KeyCode::Enter));
    // gpt-5 means nothing to a llama.cpp server: the Models page opens.
    assert_eq!(
        effects,
        vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
    );
    assert!(
        matches!(&h.app.screen, Screen::Settings(settings) if settings.section == SettingsSection::Models),
        "the switch asks for a model, on the Models page"
    );
    assert_eq!(
        h.store.load().unwrap().effective(None).model,
        None,
        "the foreign model id did not survive the switch"
    );
}

#[test]
fn d_asks_then_y_forgets_the_highlighted_instance() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |settings| {
        settings.provider = Some("llama-cpp".into());
        settings.instance = Some("home".into());
        settings.upsert_instance(
            "home",
            rhema_config::ProviderInstance {
                provider: "llama-cpp".into(),
                api_key: None,
                secret_ref: None,
                base_url: Some("http://home:8080".into()),
                api: None,
            },
        );
    });
    crate::commands::dispatch(&mut h.app, "/login");
    // d arms the confirm; the profile still holds the instance.
    h.app.on_key(key(KeyCode::Char('d')));
    {
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.instance_confirm.as_deref(), Some("home"));
    }
    assert!(
        h.app
            .config
            .effective(Some("personal"))
            .instances
            .contains_key("home"),
        "d alone removes nothing"
    );
    // n cancels, then d and y remove for real.
    h.app.on_key(key(KeyCode::Char('n')));
    h.app.on_key(key(KeyCode::Char('d')));
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(matches!(h.app.screen, Screen::Settings(_)));
    let effective = h.store.load().unwrap().effective(None);
    assert!(effective.instances.is_empty());
    assert_eq!(effective.instance, None);
    {
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.instance_confirm, None, "the confirm spent itself");
    }
    // The provider stays llama-cpp, whose keyless server always
    // constructs, on the default URL now, the way a flat profile
    // always could.
    assert!(matches!(
        h.app.backend,
        Some(Backend::LlamaCpp { ref url, .. }) if url == "http://localhost:8080"
    ));
}

#[test]
fn a_second_openai_compatible_instance_lands_under_its_own_name() {
    let mut h = harness(None, None, Session::default());
    // The custom endpoint row: its base URL, then the key.
    let name = descend_to_credential(&mut h, LoginProvider::OpenAiCompatible);
    assert_eq!(name, "Custom OpenAI-compatible API");
    for c in "http://proxy:1234/v1".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    // The request-shape choice follows the URL; enter takes the
    // highlighted chat completions, the default the route rule would
    // have picked.
    h.app.on_key(key(KeyCode::Enter));
    for c in "sk-p".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    // The choice persisted with the instance: the saved instance
    // records the shape the stage chose.
    {
        let settings = h.store.load().unwrap().effective(None);
        let instance = settings.instances.get("Custom OpenAI-compatible API");
        assert_eq!(
            instance.and_then(|i| i.api),
            Some(rhema_config::ProviderApi::Chat),
            "the chosen shape lands in the saved instance"
        );
        assert_eq!(
            instance.and_then(|i| i.base_url.as_deref()),
            Some("http://proxy:1234/v1")
        );
    }

    // A second login of the same provider suggests a fresh name.
    crate::commands::dispatch(&mut h.app, "/login");
    let name = descend_to_credential(&mut h, LoginProvider::OpenAiCompatible);
    assert_eq!(
        name, "Custom OpenAI-compatible API 2",
        "the suggestion skips the taken name"
    );
}

#[test]
fn the_copilot_login_runs_a_flow_and_persists_its_credential() {
    let mut h = harness(None, None, Session::default());
    // Open Subscriptions, walk to the GitHub Copilot row, name the
    // instance, and enter: the effect starts the flow, not a
    // credential input.
    crate::commands::dispatch(&mut h.app, "/login");
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(login.stage, crate::screens::DialogStage::Flow);
            assert_eq!(
                login.pending.as_ref().map(|p| p.provider),
                Some(LoginProvider::Copilot)
            );
        }
        _ => panic!("expected the login screen in its flow stage"),
    }

    // The flow's instruction and progress render on the screen.
    h.app
        .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Instruct(
            rhema_provider::LoginInstruction {
                text: "open the page".into(),
                url: Some("https://github.com/login/device".into()),
                code: Some("ABCD-1234".into()),
            },
        )));
    h.app
        .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Waiting(
            "waiting".into(),
        )));
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(login.flow_waiting.as_deref(), Some("waiting"));
            assert!(login.flow_instruction.is_some());
        }
        _ => panic!("expected the login screen mid-flow"),
    }

    // Completion only stores the credential; /model chooses a model.
    let effects = h.app.on_event(UiEvent::LoginUpdate(
        rhema_provider::LoginUpdate::Completed("gho_token".into()),
    ));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    let effective = h.store.load().unwrap().effective(None);
    assert_eq!(
        effective.instances["GitHub Copilot"].api_key.as_deref(),
        Some("gho_token")
    );
    assert!(effective.model.is_none());
}

#[test]
fn the_chatgpt_login_runs_a_flow_and_persists_its_credential() {
    let mut h = harness(None, None, Session::default());
    // Open the dialog and Subscriptions; ChatGPT is its first row.
    crate::commands::dispatch(&mut h.app, "/login");
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    match &h.app.screen {
        Screen::Settings(s) => {
            let login = s.dialog.as_ref().unwrap();
            assert_eq!(login.stage, crate::screens::DialogStage::Flow);
            assert_eq!(
                login.pending.as_ref().map(|p| p.provider),
                Some(LoginProvider::ChatGpt)
            );
        }
        _ => panic!("expected the login screen in its flow stage"),
    }

    // The browser flow's credential arrives as one JSON string and rides
    // the ChatGPT backend, not Copilot's.
    let effects = h.app.on_event(UiEvent::LoginUpdate(
        rhema_provider::LoginUpdate::Completed(r#"{"access_token":"a"}"#.into()),
    ));
    assert_eq!(effects, vec![]);
    assert!(matches!(h.app.screen, Screen::Chat));
    let effective = h.store.load().unwrap().effective(None);
    assert_eq!(
        effective.instances["ChatGPT"].api_key.as_deref(),
        Some(r#"{"access_token":"a"}"#)
    );
    assert!(effective.model.is_none());
}

#[test]
fn a_failed_flow_renders_its_error_and_persists_nothing() {
    let mut h = harness(None, None, Session::default());
    crate::commands::dispatch(&mut h.app, "/login");
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Down));
    h.app
        .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Failed(
            "authorization was denied on GitHub".into(),
        )));
    match &h.app.screen {
        Screen::Settings(s) => assert_eq!(
            s.dialog.as_ref().unwrap().status,
            crate::screens::DialogStatus::Error("authorization was denied on GitHub".into())
        ),
        _ => panic!("expected the login screen"),
    }
    let stored = h.store.load().unwrap();
    assert!(
        stored
            .profiles
            .values()
            .all(|profile| profile.instances.is_empty())
    );
    // Esc walks back to the categories, then closes the dialog onto the
    // Providers page for a retry.
    h.app.on_key(key(KeyCode::Esc));
    h.app.on_key(key(KeyCode::Esc));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.dialog.is_none());
}

#[test]
fn choosing_a_model_persists_it_and_builds_the_agent() {
    let mut h = harness(Some("k"), None, Session::default());
    assert!(matches!(h.app.screen, Screen::Chat));
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    h.app.on_key(ctrl('p'));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::FetchUsage]);
    // A settings page keeps its place after the pick: the model in use
    // moves under the cursor instead of the page closing.
    assert!(matches!(
        &h.app.screen,
        Screen::Settings(settings) if settings.section == SettingsSection::Models
    ));
    assert!(h.app.agent.is_some());
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-a")
    );
}

#[test]
fn ctrl_p_rebuild_preserves_the_session() {
    let session = Session {
        history: vec![Message::text(Role::User, "earlier work")],
        driver_state: serde_json::Value::Null,
        cost_usd: 0.0,
        usage_stats: serde_json::Value::Null,
    };
    let mut h = harness(Some("k"), Some("m-a"), session);
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    // Warm cache: no refetch effect.
    assert!(h.app.on_key(ctrl('p')).is_empty());
    assert!(matches!(&h.app.screen, Screen::Settings(_)));
    // The cursor opens on m-a, the model in use: one step down is m-b.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Esc));

    assert!(matches!(h.app.screen, Screen::Chat));
    let agent = h.app.agent.as_ref().expect("rebuilt agent");
    assert_eq!(
        agent.session().history,
        vec![Message::text(Role::User, "earlier work")]
    );
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-b")
    );
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("model switched to google/m-b".into()))
    );
}

#[test]
fn esc_when_idle_asks_and_y_quits() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    h.app.on_key(key(KeyCode::Esc));
    assert!(h.app.confirm_quit);
    assert!(!h.app.should_quit);
    h.app.on_key(key(KeyCode::Char('n')));
    assert!(!h.app.confirm_quit);
    h.app.on_key(key(KeyCode::Esc));
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(h.app.should_quit);
}

#[test]
fn the_context_footprint_estimates_a_loaded_history() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // No reported usage and no history: zero, as before.
    assert_eq!(h.app.context_used(), 0);

    // A resumed history has already spent context: eight characters
    // estimate to two tokens. The agent owns the live history.
    h.app
        .agent
        .as_mut()
        .unwrap()
        .session_mut()
        .history
        .push(Message::text(Role::User, "12345678"));
    assert_eq!(h.app.context_used(), 2);

    // An image prices as vision tokens, not as its base64 bytes:
    // a megabyte of screenshot data is one image on the gauge.
    h.app
        .agent
        .as_mut()
        .unwrap()
        .session_mut()
        .history
        .push(Message::text_with_images(
            Role::User,
            "see this",
            vec![rhema_core::Image::new(
                rhema_core::ImageMediaType::Png,
                "A".repeat(1_000_000),
            )],
        ));
    assert_eq!(h.app.context_used(), 2 + 2 + 1_000);

    // A reported turn wins over the estimate: prompt plus reply,
    // the shape of the next request.
    h.app.usage = Some(Usage {
        input_tokens: 500,
        output_tokens: 3,
        cached_input_tokens: 0,
    });
    assert_eq!(h.app.context_used(), 503);
}

#[test]
fn the_live_usage_tally_moves_the_gauge_mid_turn() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app
        .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);

    // The provider's running tally lands as an engine event: its
    // prompt count already covers the whole conversation, so the
    // gauge reads prompt plus the completion streamed so far.
    h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
        input_tokens: 400,
        output_tokens: 7,
        cached_input_tokens: 0,
    })));
    assert_eq!(h.app.context_used(), 407);

    // A later frame replaces the tally, never adds to it.
    h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
        input_tokens: 400,
        output_tokens: 19,
        cached_input_tokens: 0,
    })));
    assert_eq!(h.app.context_used(), 419);

    // A zero tally is a placeholder, not a measurement: the gauge
    // keeps the last real figure instead of collapsing to 0.
    h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
    })));
    assert_eq!(h.app.context_used(), 419);
}

#[test]
fn the_status_tracks_thinking_versus_running() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app
        .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);

    // The step begins: thinking.
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
    assert!(!h.app.turn.processing);

    // A tool call arrives: running it.
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
        id: "c1".into(),
        name: "bash".into(),
    }));
    assert!(h.app.turn.processing);

    // The result lands: still running, until the next step or the end.
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
        id: "c1".into(),
        content: "on main".into(),
        is_error: false,
        code: Some(0),
    }));
    assert!(h.app.turn.processing);

    // The turn ends: idle, no phase.
    let agent = HostAgent::new(HostAgentConfig::new(
        Arc::new(NeverDriven),
        "m-a",
        PathBuf::from("."),
        rhema_host_agent::AllowAll,
    ))
    .unwrap();
    h.app.on_event(UiEvent::TurnCancelled {
        agent: Box::new(agent),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.turn.processing);
}

#[test]
fn paste_lands_in_the_input_with_newlines() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_paste("a\r\nb");
    assert_eq!(h.app.chat.input, "a\nb");
}

#[test]
fn an_oversized_paste_is_cut_at_the_cap_and_noticed() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let big = "x".repeat(2 * 1024 * 1024);
    h.app.on_paste(&big);
    let input = h.app.chat.input.clone();
    assert_eq!(input.len(), 1024 * 1024, "cut at the cap");
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("truncated"))),
        "the cut is noticed"
    );
}

#[test]
fn a_mouse_down_over_the_transcript_starts_a_selection() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.transcript_view = crate::app::selection::TranscriptView {
        area: ratatui::layout::Rect::new(0, 0, 20, 4),
        top: 7,
        rows: vec!["hello".to_owned()],
        rail: None,
    };
    h.app.on_mouse_down(2, 0);
    assert_eq!(
        h.app.selection.map(|selection| selection.anchor),
        Some((2, 0)),
        "the transcript starts a selection"
    );
    assert_eq!(
        h.app.selection.map(|selection| selection.viewport_top),
        Some(7),
        "the selection remembers the row the window started on"
    );
}

#[test]
fn a_mouse_down_outside_the_drawn_rows_clears_the_selection() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.transcript_view = crate::app::selection::TranscriptView {
        area: ratatui::layout::Rect::new(0, 0, 20, 4),
        top: 0,
        rows: vec!["hello".to_owned()],
        rail: None,
    };
    h.app.on_mouse_down(2, 0);
    h.app.on_mouse_down(2, 3);
    assert!(
        h.app.selection.is_none(),
        "the blank rows below the text select nothing"
    );
}

#[test]
fn a_drag_on_the_scroll_rail_moves_the_window_not_a_selection() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.transcript_view.rail = Some(crate::app::selection::ScrollRail {
        area: ratatui::layout::Rect::new(9, 1, 1, 10),
        total: 40,
        visible: 10,
    });
    // A press at the track's first row asks for the topmost content.
    h.app.on_mouse_down(9, 1);
    assert_eq!(h.app.chat.scroll, 30);
    assert!(h.app.rail_drag, "the press arms a rail drag");
    assert!(h.app.selection.is_none(), "no selection under the rail");
    // The drag follows the pointer to the live tail.
    h.app.on_mouse_drag(9, 10);
    assert_eq!(h.app.chat.scroll, 0);
    // The release consumes the drag, so it never routes as a click.
    assert!(h.app.finish_selection(9, 10));
    assert!(!h.app.rail_drag);
}

/// A transcript view with one drawn row, `hello`, and a selection
/// covering `hell` (click at column 0, drag to column 3).
fn selected_hello(h: &mut Harness) {
    h.app.transcript_view = crate::app::selection::TranscriptView {
        area: ratatui::layout::Rect::new(0, 0, 20, 4),
        top: 0,
        rows: vec!["hello".to_owned()],
        rail: None,
    };
    h.app.on_mouse_down(0, 0);
    h.app.on_mouse_drag(3, 0);
    h.app.finish_selection(3, 0);
    assert!(h.app.selection.is_some(), "the drag leaves a selection");
}

#[test]
fn a_bare_modifier_press_keeps_the_selection() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    selected_hello(&mut h);
    h.app.on_key(KeyEvent::new(
        KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
        KeyModifiers::SHIFT,
    ));
    assert!(
        h.app.selection.is_some(),
        "a modifier alone is not a command and must not erase the highlight"
    );
}

#[test]
fn ctrl_shift_c_copies_the_selection_instead_of_quitting() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    selected_hello(&mut h);
    // The kitty alternate-key parse leaves the shifted C under CONTROL.
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL));
    assert!(!h.app.should_quit, "the copy key does not quit");
    assert!(
        h.app.selection.is_some(),
        "the highlight stays after a copy"
    );
}

#[test]
fn ctrl_shift_c_from_a_terminal_that_keeps_shift_copies() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    selected_hello(&mut h);
    h.app.on_key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(!h.app.should_quit, "the copy key does not quit");
    assert!(
        h.app.selection.is_some(),
        "the highlight stays after a copy"
    );
}

#[test]
fn ctrl_c_copies_and_never_quits() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    selected_hello(&mut h);
    h.app.on_key(ctrl('c'));
    assert!(!h.app.should_quit, "Ctrl-C is the copy key, not quit");
    assert!(
        h.app.selection.is_some(),
        "the highlight stays after a copy"
    );
}

#[test]
fn ctrl_c_with_nothing_selected_is_inert() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_key(ctrl('c'));
    assert!(!h.app.should_quit, "a stray Ctrl-C must not quit");
    assert_eq!(h.app.chat.input, "", "and must not type anything");
}

#[test]
fn altgr_c_keeps_typing_instead_of_copying() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    assert!(!h.app.should_quit);
    assert_eq!(
        h.app.chat.input, "c",
        "AltGr composes text; Ctrl-Alt-C is not a copy"
    );
}

#[test]
fn a_key_clears_the_selection() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.transcript_view = crate::app::selection::TranscriptView {
        area: ratatui::layout::Rect::new(0, 0, 20, 4),
        top: 0,
        rows: vec!["hello".to_owned()],
        rail: None,
    };
    h.app.on_mouse_down(2, 0);
    h.app.on_key(key(KeyCode::Char('x')));
    assert!(h.app.selection.is_none());
}

#[test]
fn the_wheel_scrolls_the_same_step_as_the_page_keys() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_mouse_scroll(true);
    assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
    h.app.on_key(key(KeyCode::PageUp));
    assert_eq!(
        h.app.chat.scroll,
        2 * crate::screens::ChatState::SCROLL_STEP
    );
    h.app.on_mouse_scroll(false);
    assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
}

#[test]
fn esc_when_running_cancels_the_turn_and_flags_the_channel() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    h.app.turn.cancel = Some(cancel);
    h.app.turn.running = true;
    h.app.on_key(key(KeyCode::Esc));
    assert!(h.app.turn.decline_all, "upcoming approvals decline");
    assert!(!h.app.should_quit, "esc is not quit mid-turn");
    assert!(*cancelled.borrow(), "the turn task sees the flag");

    // The task's event closes the turn out.
    let agent = HostAgent::new(HostAgentConfig::new(
        Arc::new(NeverDriven),
        "m-a",
        PathBuf::from("."),
        rhema_host_agent::AllowAll,
    ))
    .unwrap();
    h.app.on_event(UiEvent::TurnCancelled {
        agent: Box::new(agent),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.turn.running);
    assert!(h.app.turn.cancel.is_none());
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("cancelled")
    ));
}

#[test]
fn esc_with_a_pending_modal_cancels_and_releases_the_blocked_call() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (hook, answers) = ScriptedApproval::pair(tx);
    let handle = std::thread::spawn(move || hook.approve(&test_call()));
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app.begin_turn(
        cancel,
        tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0,
        Some(answers),
    );
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    h.app.turn.cancel = Some(cancel);
    h.app.turn.running = true;

    let event = rx.blocking_recv().expect("the parked call requested");
    h.app.on_event(event);
    assert!(h.app.turn.pending_approval.is_some());
    h.app.on_key(key(KeyCode::Esc));
    assert!(!handle.join().unwrap(), "the parked call was declined");
    assert!(
        *cancelled.borrow(),
        "the turn was cancelled, not just declined"
    );
    assert!(h.app.turn.pending_approval.is_none());
}

#[test]
fn the_cancelled_event_hands_the_agent_back_with_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    let agent = HostAgent::new(HostAgentConfig::new(
        Arc::new(NeverDriven),
        "m-a",
        PathBuf::from("."),
        rhema_host_agent::AllowAll,
    ))
    .unwrap();
    h.app.on_event(UiEvent::TurnCancelled {
        agent: Box::new(agent),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.turn.running);
    assert!(h.app.agent.is_some());
    assert!(h.app.turn.cancel.is_none());
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("cancelled")
    ));
}

#[test]
fn the_modal_answer_flows_back_to_the_blocked_turn() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (hook, answers) = ScriptedApproval::pair(tx);
    let handle = std::thread::spawn(move || hook.approve(&test_call()));
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app.begin_turn(
        cancel,
        tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0,
        Some(answers),
    );

    let event = rx.blocking_recv().expect("the parked call requested");
    h.app.on_event(event);
    assert!(h.app.turn.pending_approval.is_some());
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(handle.join().unwrap(), "the y key approves the parked call");
}

#[test]
fn turn_finished_records_usage_and_errors() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let make_agent = || {
        Box::new(
            HostAgent::new(HostAgentConfig::new(
                Arc::new(NeverDriven),
                "m-a",
                ".",
                rhema_host_agent::AllowAll,
            ))
            .unwrap(),
        )
    };
    let usage = Usage {
        input_tokens: 12,
        output_tokens: 3,
        cached_input_tokens: 4,
    };
    h.app.turn.running = true;
    h.app.on_event(UiEvent::TurnFinished {
        agent: make_agent(),
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage,
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.turn.running);
    assert!(h.app.error.is_none());
    assert_eq!(h.app.usage, Some(usage));

    h.app.turn.running = true;
    h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Err("stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit".into()),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
    assert!(!h.app.turn.running);
    assert_eq!(
        h.app.error.as_deref(),
        Some(
            "stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit"
        )
    );
}

#[test]
fn chat_submit_guards() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    // An empty input never submits.
    assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(!h.app.turn.running);
    // While a turn runs, typing works and Enter steers: the input
    // clears, the user line shows, and the effect notes the
    // interruption on the status strip.
    h.app.turn.running = true;
    for c in "hi".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(h.app.chat.input, "hi");
    assert_eq!(
        h.app.on_key(key(KeyCode::Enter)),
        vec![Effect::SteerTurn("hi".into())]
    );
    assert!(h.app.turn.running);
    assert!(h.app.chat.input.is_empty());
    // Idle again: Enter submits a fresh turn, shows the user line,
    // and blocks re-entry.
    h.app.turn.running = false;
    for c in "hi".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        h.app.on_key(key(KeyCode::Enter)),
        vec![Effect::StartTurn {
            spans: Vec::new(),
            text: "hi".into(),
            images: Vec::new()
        }]
    );
    assert!(h.app.turn.running);
    assert_eq!(
        h.app.transcript.lines().first(),
        Some(&Line::User {
            pastes: Vec::new(),
            text: "hi".into(),
            images: Vec::new()
        })
    );
    // An empty input still never submits or steers.
    assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
}

#[test]
fn a_submit_carries_the_pastes_payload_and_its_span() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let long = "l1\nl2\nl3\nl4\nl5";
    h.app.chat.paste(long);
    let spans = vec![crate::paste::PasteSpan {
        start: 0,
        end: long.len(),
        lines: 5,
    }];
    // The turn starts on the plain text, and the transcript line
    // carries the span its chip stands for.
    assert_eq!(
        h.app.on_key(key(KeyCode::Enter)),
        vec![Effect::StartTurn {
            text: long.to_owned(),
            images: Vec::new(),
            spans: spans.clone(),
        }]
    );
    assert_eq!(
        h.app.transcript.lines().first(),
        Some(&Line::User {
            pastes: spans,
            text: long.to_owned(),
            images: Vec::new()
        })
    );
}

/// The paste key runs its read off the key handler: the key only
/// asks, and the grab lands back as an event that inserts the paste
/// or the notice the failure carries.
#[test]
fn the_paste_key_spawns_off_the_handler_and_lands_as_an_event() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let paste = ctrl('v');
    assert_eq!(h.app.on_key(paste), vec![Effect::GrabClipboard]);

    // The grab's arrival inserts exactly what the synchronous read
    // did: text into the box, a failure's notice into the transcript.
    h.app.on_event(UiEvent::ClipboardGrabbed {
        grab: crate::screens::clipboard::Grab::Text("pasted".to_owned()),
    });
    assert!(h.app.chat.input.contains("pasted"));
    h.app.on_event(UiEvent::ClipboardGrabbed {
        grab: crate::screens::clipboard::Grab::Oversized(6 * 1024 * 1024),
    });
    let last = h.app.transcript.lines().last();
    assert!(
        matches!(last, Some(Line::Notice(text)) if text.contains("too large")),
        "the notice lands: {last:?}"
    );
}

/// A mid-turn settings toggle queued its rebuild for the turn's end:
/// the queue only drains when the agent returns, so the prompt the
/// toggles changed reaches the very next turn instead of waiting for
/// some unrelated rebuild.
#[test]
fn a_rebuild_promised_mid_turn_lands_at_the_turns_end() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (steer, _) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app.begin_turn(cancel, steer, None);
    // The spawn takes the agent out with the turn, as the run loop's
    // StartTurn does.
    let agent = Box::new(h.app.agent.take().unwrap());
    // The toggle's rebuild finds the agent checked out with the
    // running turn: the promise queues, and a stronger stamp a second
    // mid-turn ask carries does not degrade.
    assert!(h.app.rebuild_live_agent(false).is_ok());
    assert_eq!(
        h.app.deferred_rebuild,
        Some(false),
        "the promise queues while the agent is out"
    );
    assert!(h.app.rebuild_live_agent(true).is_ok());
    assert_eq!(
        h.app.deferred_rebuild,
        Some(true),
        "the stronger stamp survives the queue"
    );

    // The turn finishes and the agent returns: the drain runs the
    // promised rebuild, and the promise is spent.
    let (_, rx) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: rx,
    });
    assert_eq!(h.app.deferred_rebuild, None, "the promise is spent");
}

/// Two picks in flight: the newest owns the landing, and the first
/// one home does not overwrite the conversation the picker moved to.
/// The loads land out of order, the older one after the newer one
/// already applied.
#[test]
fn a_stale_resume_overwritten_by_a_newer_pick_drops() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let dir = tempfile::tempdir().unwrap();
    let mut first = Session::default();
    first
        .history
        .push(Message::text(Role::User, "the first pick"));
    first.cost_usd = 1.0;
    let mut second = Session::default();
    second
        .history
        .push(Message::text(Role::User, "the second pick"));
    second.cost_usd = 2.5;
    let first_path = dir.path().join("first.json");
    let second_path = dir.path().join("second.json");
    crate::sessions::write_session(&first_path, &first).unwrap();
    crate::sessions::write_session(&second_path, &second).unwrap();

    // Pick one, then the other before the first lands.
    h.app.request_resume(&first_path);
    h.app.request_resume(&second_path);
    // The newer pick lands first and applies.
    let second_load = crate::app::App::load_resume_parts(&second_path).map_err(|e| e.to_string());
    h.app.on_event(UiEvent::SessionResumed {
        path: second_path.clone(),
        result: second_load,
    });
    assert_eq!(h.app.session_cost, 2.5, "the newer pick owns the landing");
    assert_eq!(h.app.resume_pending, None, "its landing spent the request");

    // The older pick lands late: it drops, overwriting nothing.
    let first_load = crate::app::App::load_resume_parts(&first_path).map_err(|e| e.to_string());
    h.app.on_event(UiEvent::SessionResumed {
        path: first_path,
        result: first_load,
    });
    assert_eq!(h.app.session_cost, 2.5, "the stale landing changes nothing");
    assert_eq!(
        h.app
            .transcript
            .lines()
            .iter()
            .filter(|line| matches!(line, Line::Notice(text) if text.contains("resumed")))
            .count(),
        1,
        "only the newer pick announced its resume"
    );
}

/// A load overtaken by a turn refuses the way a fresh pick would, the
/// landing would otherwise install a second agent under the running
/// turn'"'"'s own.
#[test]
fn a_resume_that_lands_mid_turn_is_refused() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::default();
    session.history.push(Message::text(Role::User, "late"));
    let path = dir.path().join("late.json");
    crate::sessions::write_session(&path, &session).unwrap();

    h.app.request_resume(&path);
    // A turn begins before the load lands: the key handler stays
    // live while the blocking pool still holds the file.
    let (steer, _) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app.begin_turn(cancel, steer, None);
    let loaded = crate::app::App::load_resume_parts(&path).map_err(|e| e.to_string());
    h.app.on_event(UiEvent::SessionResumed {
        path: path.clone(),
        result: loaded,
    });
    assert_eq!(
        h.app.transcript.lines().last(),
        Some(&Line::Notice(
            "a turn began before the session finished loading; /resume again once it ends"
                .to_owned()
        )),
        "the landing refuses with the re-pick named"
    );
    assert_eq!(h.app.resume_pending, None, "the request is spent");
    assert_ne!(
        h.app.session_file.as_deref(),
        Some(path.as_path()),
        "nothing of the loaded session applied"
    );
}

/// A picker Enter under a running background turn is declined the
/// way every other interactive switch is: the pick would apply over
/// the stale pre-turn snapshot and the turn'"'"'s end would then hand
/// the old conversation'"'"'s agent back on top of it.
#[test]
fn a_model_pick_under_a_running_turn_is_declined() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_key(ctrl('p'));
    assert!(
        matches!(&h.app.screen, Screen::Settings(settings) if settings.section == SettingsSection::Models),
        "the Models page sits open for the pick"
    );
    h.app.turn.running = true;
    let effects = h.app.choose_model("google", Some("google"), "m-z");
    assert!(effects.is_empty(), "the declined pick changes nothing");
    assert_eq!(
        h.app.transcript.lines().last(),
        Some(&Line::Notice(
            "a turn is running; the model pick waits for it to finish".to_owned()
        ))
    );
    assert!(
        matches!(&h.app.screen, Screen::Settings(settings) if settings.section == SettingsSection::Models),
        "the page stays up; the pick can be made again at idle"
    );
    assert_ne!(
        h.app.effective().model.as_deref(),
        Some("m-z"),
        "the model did not move under the running turn"
    );
}

/// The paste key's unreachable notice lands in the transcript: the
/// chat screen has no notice channel of its own, the app owns the
/// transcript.
#[test]
fn a_paste_notice_from_the_chat_lands_in_the_transcript() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let notice = crate::screens::clipboard::UNREACHABLE_NOTICE.to_owned();
    let effects = h.app.on_chat_key(
        key(KeyCode::Char('v')),
        crate::screens::ChatAction::Notice {
            text: notice.clone(),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(h.app.transcript.lines().last(), Some(&Line::Notice(notice)));
}

/// A command with an image attached is a prompt with an image, not
/// a command: images route to a turn unconditionally.
#[test]
fn a_submission_carrying_images_is_never_a_command() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
    h.app.chat.input = format!("/clear {}", ChatState::MARK);
    h.app.chat.chips = vec![crate::screens::Chip::Image(image.clone())];
    h.app.chat.cursor = h.app.chat.input.chars().count();
    match h.app.on_key(key(KeyCode::Enter)).as_slice() {
        [Effect::StartTurn { text, images, .. }] => {
            assert_eq!(text, "/clear ");
            assert_eq!(images.first(), Some(&image));
        }
        other => panic!("an image-bearing submit is a turn, not a command: {other:?}"),
    }
    assert_eq!(
        h.app.transcript.lines().last(),
        Some(&Line::User {
            pastes: Vec::new(),
            text: "/clear ".into(),
            images: vec![image],
        })
    );
}

/// Steering stays text: an image mid-turn keeps everything in the
/// box for the next submit rather than stripping the image from a
/// nudge.
#[test]
fn a_full_steer_queue_refuses_the_nudge_and_keeps_the_line() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let (steer, _rx) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app.begin_turn(cancel, steer, None);
    for i in 0..STEER_QUEUE_CAP {
        h.app.chat.input = format!("nudge {i}");
        let effects = h.app.steer_turn(h.app.chat.input.clone());
        assert_eq!(effects.len(), 1, "queued nudge {i} still steers");
    }
    // The queue is full: the next nudge returns to the input untouched,
    // cursor where it was.
    h.app.chat.input = "one too many".into();
    h.app.chat.cursor = "one too many".chars().count();
    let effects = h.app.steer_turn("one too many".into());
    assert!(effects.is_empty(), "a full queue refuses: {effects:?}");
    assert_eq!(h.app.chat.input, "one too many");
    assert_eq!(
        h.app.chat.cursor,
        "one too many".chars().count(),
        "the cursor stays where the line left it"
    );

    // A paste rides an anchor: the refusal keeps the marked form and
    // its chip, not the expanded text with the payload stranded.
    let marked = format!("look at {} here", ChatState::MARK);
    h.app.chat.input = marked.clone();
    h.app.chat.chips = vec![crate::screens::Chip::Paste("the hidden payload".to_owned())];
    let effects = h.app.steer_turn("look at the hidden payload here".into());
    assert!(effects.is_empty());
    assert_eq!(h.app.chat.input, marked, "the anchors stay");
    assert!(
        matches!(h.app.chat.chips.as_slice(), [crate::screens::Chip::Paste(paste)] if paste == "the hidden payload"),
        "the chip still holds its payload: {:?}",
        h.app.chat.chips
    );
}

#[test]
fn an_image_cannot_steer_a_running_turn() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
    h.app.chat.insert_str("look");
    h.app.chat.paste_image(image);
    assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(!h.app.chat.input.is_empty(), "the text stays");
    assert_eq!(h.app.chat.chips.len(), 1, "the image stays");
    assert!(h.app.turn.running);
}

/// A running command's output fills the preview buffer from its
/// first chunk; the next call or the result takes it back.
#[test]
fn a_running_commands_output_previews_and_goes_with_its_result() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
        id: "c1".into(),
        name: "bash".into(),
    }));
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
        id: "c1".into(),
        chunk: "one\n".into(),
    }));
    assert_eq!(h.app.turn.tool_output, "one\n");
    assert!(
        h.app.turn.tool_output_at.is_some(),
        "the first chunk clocks in"
    );

    // The next command takes the last one's preview with it.
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
        id: "c2".into(),
        name: "bash".into(),
    }));
    assert!(h.app.turn.tool_output.is_empty());
    assert!(h.app.turn.tool_output_at.is_none());

    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
        id: "c2".into(),
        chunk: "two\n".into(),
    }));
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
        id: "c2".into(),
        content: "ok".into(),
        is_error: false,
        code: Some(0),
    }));
    assert!(h.app.turn.tool_output.is_empty());
    assert!(h.app.turn.tool_output_at.is_none());
}

#[test]
fn engine_events_stream_into_the_transcript() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::TextDelta("partial".into())));
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::TextDelta(" answer".into())));
    assert_eq!(
        h.app.transcript.lines(),
        &[Line::Assistant("partial answer".into())]
    );
}

#[test]
fn reasoning_previews_and_vanishes_when_output_starts() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("hmm".into())));
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta(" deeper".into())));
    assert_eq!(h.app.turn.thinking, "hmm deeper");

    // Real output clears the buffer: the block vanishes.
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::TextDelta("the answer".into())));
    assert!(h.app.turn.thinking.is_empty());

    // So does a tool call mid-turn.
    h.app
        .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("again".into())));
    h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
        id: "c1".into(),
        name: "bash".into(),
    }));
    assert!(h.app.turn.thinking.is_empty());
}

#[test]
fn ctrl_d_quits_from_any_screen_and_ctrl_c_does_not() {
    let mut h = harness(None, None, Session::default());
    h.app.on_key(ctrl('c'));
    assert!(!h.app.should_quit, "Ctrl-C copies, it does not quit");

    let mut h = harness(None, None, Session::default());
    h.app.on_key(ctrl('d'));
    assert!(h.app.should_quit);
}

#[test]
fn login_success_writes_into_the_selected_tab_profile() {
    let mut h = harness(None, None, Session::default());
    // Give the config a second profile so the login has a work tab.
    h.app
        .config
        .profiles
        .insert("work".to_owned(), rhema_config::Profile::default());
    // The session moves to the work profile, whose Providers page
    // the settings then edit, and an openrouter instance lands there.
    crate::commands::dispatch(&mut h.app, "/profile work");
    crate::commands::dispatch(&mut h.app, "/login");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.profile, "work");
    descend_to_credential(&mut h, LoginProvider::OpenRouter);
    for c in "work-key".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![]);
    // The session now runs on work, and the key landed in its section.
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    let stored = h.store.load().unwrap();
    let work = stored.effective(Some("work"));
    assert_eq!(work.instance.as_deref(), Some("OpenRouter"));
    assert_eq!(
        work.instances["OpenRouter"].api_key.as_deref(),
        Some("work-key")
    );
    assert!(work.model.is_none(), "the add chose no model");
}

#[test]
fn slash_profile_with_a_name_switches_the_backend() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.config.profiles.insert(
        "work".to_owned(),
        rhema_config::Profile {
            model: Some("work-model".into()),
            ..keyed_profile("openrouter", "work-key")
        },
    );
    crate::commands::dispatch(&mut h.app, "/profile work");
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    assert!(matches!(h.app.screen, Screen::Chat), "stays in the chat");
    match &h.app.backend {
        Some(Backend::OpenRouter { api_key }) => assert_eq!(api_key, "work-key"),
        other => panic!("expected the openrouter backend, got {other:?}"),
    }
    // The profiles' sections are untouched, but the explicit choice
    // is recorded for this workspace right away.
    let stored = h.store.load().unwrap();
    assert!(stored.profiles.contains_key("personal"));
    assert_eq!(
        stored.last_profile_in(&h.app.options.workspace).as_deref(),
        Some("work")
    );
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("switched to profile work".into()))
    );
}

#[test]
fn startup_without_a_flag_starts_on_a_ready_profile() {
    let work_ready = rhema_config::Profile {
        model: Some("work-model".into()),
        ..keyed_profile("openrouter", "work-key")
    };
    // personal is empty, work is logged in: the session starts on
    // work instead of the login screen.
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    assert_eq!(
        startup_profile(&config, Path::new("/nowhere"), None),
        "work"
    );
    // personal can serve turns: the default stands.
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed_profile("google", "k")),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    assert_eq!(
        startup_profile(&config, Path::new("/nowhere"), None),
        "personal"
    );
    // Nothing can: the default stands and the login screen opens on
    // it.
    assert_eq!(
        startup_profile(&Config::default(), Path::new("/nowhere"), None),
        "personal"
    );
    // A key without a provider name is not usable either: nothing
    // defaults to google, so the logged-out-looking profile loses to
    // the ready one.
    let mut keyed = rhema_config::Profile::default();
    store_key(&mut keyed, "google", "k");
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed),
                ("work".to_owned(), work_ready),
            ]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    assert_eq!(
        startup_profile(&config, Path::new("/nowhere"), None),
        "work"
    );
}

#[test]
fn a_workspace_reopens_on_the_profile_it_last_used() {
    let ready = |key: &str| keyed_profile("google", key);
    let mut config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), ready("k")),
                ("work".to_owned(), ready("w")),
            ]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    // Both profiles can serve turns, and proj last ran on work: the
    // workspace memory wins over the personal default.
    config.note_profile_in(Path::new("/proj"), "work");
    assert_eq!(startup_profile(&config, Path::new("/proj"), None), "work");
    // The project's choice reaches its subfolders by default.
    assert_eq!(
        startup_profile(&config, Path::new("/proj/src"), None),
        "work"
    );
    assert_eq!(
        startup_profile(&config, Path::new("/proj/src/deep"), None),
        "work"
    );
    // A sibling is not a subfolder, and another directory has no
    // memory: the default stands.
    assert_eq!(
        startup_profile(&config, Path::new("/proj-other"), None),
        "personal"
    );
    assert_eq!(
        startup_profile(&config, Path::new("/other"), None),
        "personal"
    );
    // A memory naming a profile that can no longer serve turns falls
    // through to the default, never the login screen by surprise.
    config
        .profiles
        .insert("empty".to_owned(), rhema_config::Profile::default());
    config.note_profile_in(Path::new("/proj"), "empty");
    assert_eq!(
        startup_profile(&config, Path::new("/proj"), None),
        "personal"
    );
}

#[test]
fn a_switch_in_a_subfolder_overrides_the_inherited_profile() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "work-key"));
    // The project chose work; this session is in one of its subfolders
    // and is about to choose personal there.
    h.app.options.workspace = PathBuf::from("/proj/src");
    h.app.config.note_profile_in(Path::new("/proj"), "work");
    h.app.session_profile = Some("work".to_owned());
    crate::commands::dispatch(&mut h.app, "/profile personal");

    // The subfolder's own record wins for its subtree...
    let stored = h.store.load().unwrap();
    assert_eq!(
        stored.last_profile_in(Path::new("/proj/src")).as_deref(),
        Some("personal")
    );
    assert_eq!(
        stored
            .last_profile_in(Path::new("/proj/src/deep"))
            .as_deref(),
        Some("personal")
    );
    // ...and leaves the project's record and its other subtrees alone.
    assert_eq!(
        stored.last_profile_in(Path::new("/proj")).as_deref(),
        Some("work")
    );
    assert_eq!(
        stored.last_profile_in(Path::new("/proj/docs")).as_deref(),
        Some("work")
    );
}

/// A profile whose `sandbox` section stores exactly this kind.
fn sandbox_kind_profile(kind: &str) -> rhema_config::Profile {
    rhema_config::Profile {
        sandbox: Some(rhema_config::SandboxSettings {
            kind: Some(kind.to_owned()),
            ..rhema_config::SandboxSettings::default()
        }),
        ..rhema_config::Profile::default()
    }
}

/// What this machine offers, spelled per test.
fn offered(
    docker: bool,
    lxd: bool,
    ecrou: bool,
    ssh: bool,
    http: bool,
) -> crate::sandbox::SandboxAvailability {
    crate::sandbox::SandboxAvailability {
        docker,
        lxd,
        ecrou,
        ssh,
        http,
    }
}

#[test]
fn startup_sandbox_prefers_the_stored_kind_while_it_is_offered() {
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([(
                rhema_config::DEFAULT_PROFILE.to_owned(),
                sandbox_kind_profile("docker"),
            )]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    let profile = config.effective(None);
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Docker
    );
    // The env-wins tier is injected the way the resolve tests inject
    // it: as the layered value `sandbox_kind` would have returned
    // with RHEMA_SANDBOX set (env over a stored local).
    let layered = sandbox_kind_profile("http");
    assert_eq!(
        startup_sandbox(
            &config,
            &layered,
            Path::new("/proj"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Http
    );
    // A spelling nothing answers to reads as unset: local.
    let typo = sandbox_kind_profile("dockre");
    assert_eq!(
        startup_sandbox(
            &config,
            &typo,
            Path::new("/proj"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Local
    );
}

#[test]
fn startup_sandbox_falls_through_an_unoffered_stored_kind_to_the_remembered_one() {
    let mut config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([("default".to_owned(), sandbox_kind_profile("docker"))]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    config.note_sandbox_in(Path::new("/proj"), "ssh");
    let profile = config.effective(None);
    // Docker is not offered here; the workspace's remembered ssh is.
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(false, true, false, true, false)
        ),
        SandboxChoice::Ssh
    );
    // The remembered kind is offered nowhere either: local, and the
    // stored docker kind never stands a failed container up.
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(false, true, false, false, false)
        ),
        SandboxChoice::Local
    );
}

#[test]
fn startup_sandbox_reads_the_workspace_memory_only_when_offered() {
    let mut config = Config::default();
    config.note_sandbox_in(Path::new("/proj"), "http");
    let profile = rhema_config::Profile::default();
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(false, true, false, false, true)
        ),
        SandboxChoice::Http
    );
    // A memory of a kind no machine answers to, and an unparseable
    // one besides, both fall through to local.
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(false, true, false, false, false)
        ),
        SandboxChoice::Local
    );
    config.note_sandbox_in(Path::new("/proj"), "nonsense");
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Local
    );
    // Another workspace has no memory at all.
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/other"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Local
    );
}

#[test]
fn zero_config_starts_local_even_on_a_machine_that_offers_everything() {
    let config = Config::default();
    assert_eq!(
        startup_sandbox(
            &config,
            &rhema_config::Profile::default(),
            Path::new("/proj"),
            &offered(true, true, false, true, true)
        ),
        SandboxChoice::Local
    );
}

#[test]
fn startup_sandbox_reads_the_real_env_through_sandbox_kind() {
    // The profile path runs only when the suite exports nothing:
    // the real environment decides, and the pure test above carries
    // the env-wins coverage (the edition forbids writing the var).
    if std::env::var_os("RHEMA_SANDBOX").is_some() {
        return;
    }
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([(
                rhema_config::DEFAULT_PROFILE.to_owned(),
                sandbox_kind_profile("docker"),
            )]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    let profile = config.effective(None);
    assert_eq!(
        startup_sandbox(
            &config,
            &profile,
            Path::new("/proj"),
            &offered(true, true, false, false, false)
        ),
        SandboxChoice::Docker
    );
}

#[test]
fn choosing_a_sandbox_stores_the_kind_notes_the_workspace_and_rebuilds() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // The session runs on docker (as an assembled startup would
    // have left it): switching to local must drop the assembly,
    // store the kind, remember the workspace, and rebuild inline.
    h.app.sandboxed = Some(crate::sandbox::Sandboxed {
        choice: SandboxChoice::Docker,
        target: "rhema-sandbox-proj".to_owned(),
        tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
        workspace: "/workspace".to_owned(),
        skills: Vec::new(),
        instance_lock: None,
        managed: None,
        shared: None,
        session: Default::default(),
    });
    h.app.sandbox_offered = offered(true, true, false, false, false);
    let effects = h.app.choose_sandbox(SandboxChoice::Local);
    assert!(effects.is_empty(), "local rebuilds inline: {effects:?}");
    assert!(h.app.sandboxed.is_none());
    assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
    let stored = h.store.load().unwrap();
    assert_eq!(
        stored.effective(Some("personal")).sandbox_kind().as_deref(),
        Some("local"),
        "the kind persists on the profile's sandbox section"
    );
    assert_eq!(
        stored.last_sandbox_in(Path::new(".")).as_deref(),
        Some("local"),
        "the workspace remembers the kind"
    );
}

#[test]
fn choosing_a_non_local_sandbox_assembles_through_an_effect() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(true, true, false, false, false);
    let effects = h.app.choose_sandbox(SandboxChoice::Docker);
    assert_eq!(
        effects,
        vec![Effect::AssembleSandbox(SandboxChoice::Docker)]
    );
    assert_eq!(h.app.assembling, Some(SandboxChoice::Docker));
    // The choice is already durable: a crash before the assembly
    // lands must not lose it.
    let stored = h.store.load().unwrap();
    assert_eq!(
        stored.effective(Some("personal")).sandbox_kind().as_deref(),
        Some("docker")
    );
    assert_eq!(
        stored.last_sandbox_in(Path::new(".")).as_deref(),
        Some("docker")
    );
    // The assembly's answer swaps the tools in and rebuilds.
    let assembled = crate::sandbox::Sandboxed {
        choice: SandboxChoice::Docker,
        target: "rhema-sandbox-proj".to_owned(),
        tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/host/ws")),
        workspace: "/host/ws".to_owned(),
        skills: Vec::new(),
        instance_lock: None,
        managed: None,
        shared: None,
        session: Default::default(),
    };
    let effects = h.app.on_event(crate::events::UiEvent::SandboxAssembled {
        choice: SandboxChoice::Docker,
        result: Ok(assembled),
    });
    assert!(effects.is_empty());
    assert_eq!(h.app.assembling, None);
    assert_eq!(h.app.active_sandbox(), SandboxChoice::Docker);
    assert!(
        h.app
            .agent
            .as_ref()
            .expect("the agent is rebuilt")
            .system()
            .is_some_and(|prompt| prompt.contains("/host/ws")),
        "the prompt names the sandbox-side workspace"
    );
}

#[test]
fn a_failed_assembly_reports_in_band_and_keeps_the_running_tools() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(false, true, false, false, true);
    assert_eq!(
        h.app.choose_sandbox(SandboxChoice::Http),
        vec![Effect::AssembleSandbox(SandboxChoice::Http)]
    );
    h.app.on_event(crate::events::UiEvent::SandboxAssembled {
        choice: SandboxChoice::Http,
        result: Err("connection refused".to_owned()),
    });
    assert_eq!(h.app.assembling, None);
    assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
    // The transcript says what failed; the agent never dropped.
    let notice = h
        .app
        .transcript
        .lines()
        .iter()
        .rev()
        .find_map(|line| match line {
            crate::transcript::Line::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(notice.contains("http"), "{notice}");
    assert!(notice.contains("connection refused"), "{notice}");
    assert!(h.app.agent.is_some());
}

#[test]
fn a_sandbox_this_machine_does_not_offer_is_declined_in_band() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = crate::sandbox::SandboxAvailability::default();
    assert!(h.app.choose_sandbox(SandboxChoice::Docker).is_empty());
    assert!(h.app.sandboxed.is_none());
    // The stored kind is untouched, and nothing was remembered.
    let stored = h.store.load().unwrap();
    assert_eq!(stored.effective(Some("personal")).sandbox_kind(), None);
    assert_eq!(stored.last_sandbox_in(Path::new(".")), None);
}

#[test]
fn an_unknown_sandbox_spelling_is_declined_in_band() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/sandbox dockre");
    let stored = h.store.load().unwrap();
    assert_eq!(stored.effective(Some("personal")).sandbox_kind(), None);
}

#[test]
fn slash_sandbox_with_an_instance_name_runs_bash_in_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(true, true, false, true, false);
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.place_sandbox_instance(
            "dev",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("rhema-dev".to_owned()),
                vm: None,
            },
        );
    });
    h.store.save(&h.app.config).unwrap();
    let effects = crate::commands::dispatch(&mut h.app, "/sandbox dev");
    assert_eq!(
        effects,
        vec![Effect::AssembleSandbox(SandboxChoice::Docker)],
        "the instance's kind is what assembles"
    );
    let stored = h.store.load().unwrap();
    let sandbox = stored
        .effective(Some("personal"))
        .sandbox
        .expect("the section lands");
    assert_eq!(sandbox.kind.as_deref(), Some("docker"));
    assert_eq!(
        sandbox.active.as_deref(),
        Some("dev"),
        "the pointer names the instance"
    );
}

#[test]
fn slash_sandbox_with_a_kind_still_selects_the_kind() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(true, true, false, false, false);
    let effects = crate::commands::dispatch(&mut h.app, "/sandbox docker");
    assert_eq!(
        effects,
        vec![Effect::AssembleSandbox(SandboxChoice::Docker)]
    );
    let stored = h.store.load().unwrap();
    let sandbox = stored
        .effective(Some("personal"))
        .sandbox
        .expect("the section lands");
    assert_eq!(sandbox.kind.as_deref(), Some("docker"));
    assert_eq!(
        sandbox.active, None,
        "a kind select never points at an instance"
    );
}

#[test]
fn an_unknown_sandbox_name_names_the_instances_and_the_kinds() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/sandbox nowhere");
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("nowhere")
    ));
}

#[test]
fn a_profile_switch_moves_bash_to_the_new_profiles_sandbox() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "o"));
    h.app.sandbox_machines.docker = true;
    h.app.sandbox_offered = offered(true, true, false, false, false);
    // The session runs on the current profile's docker instance.
    h.app.sandboxed = Some(crate::sandbox::Sandboxed {
        choice: SandboxChoice::Docker,
        target: "work-box".to_owned(),
        tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
        workspace: "/workspace".to_owned(),
        skills: Vec::new(),
        instance_lock: None,
        managed: None,
        shared: None,
        session: Default::default(),
    });
    // work (moving in) stores nothing: its own sandbox is local.
    let effects = h.app.switch_profile("work");
    assert!(h.app.sandboxed.is_none(), "the docker assembly leaves");
    assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::AssembleSandbox(_))),
        "local needs no assembly: {effects:?}"
    );
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("bash runs on this host")
    ));
}

#[test]
fn a_profile_switch_assembles_the_profile_moving_in() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_machines.docker = true;
    h.app.sandbox_offered = offered(true, true, false, false, false);
    // The session runs local; work stores its own docker instance.
    let mut work = keyed_profile("openrouter", "o");
    work.sandbox = Some(rhema_config::SandboxSettings {
        kind: Some("docker".to_owned()),
        active: Some("work-box".to_owned()),
        ..rhema_config::SandboxSettings::default()
    });
    work.place_sandbox_instance(
        "work-box",
        rhema_config::SandboxInstance {
            kind: "docker".to_owned(),
            target: Some("work-box".to_owned()),
            vm: None,
        },
    );
    h.app.config.profiles.insert("work".to_owned(), work);
    let effects = h.app.switch_profile("work");
    assert_eq!(h.app.assembling, Some(SandboxChoice::Docker));
    assert_eq!(
        effects,
        vec![
            Effect::FetchUsage,
            Effect::AssembleSandbox(SandboxChoice::Docker)
        ],
        "the assembly effect rides the switch"
    );
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("standing up the docker sandbox")
    ));
}

#[test]
fn a_switch_keeps_a_sandbox_the_new_profile_resolves_to() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_machines.docker = true;
    h.app.sandbox_offered = offered(true, true, false, false, false);
    h.app.sandboxed = Some(crate::sandbox::Sandboxed {
        choice: SandboxChoice::Docker,
        target: "work-box".to_owned(),
        tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
        workspace: "/workspace".to_owned(),
        skills: Vec::new(),
        instance_lock: None,
        managed: None,
        shared: None,
        session: Default::default(),
    });
    // work resolves to the same kind AND the same target: the
    // running assembly is already the profile's own sandbox.
    let mut work = keyed_profile("openrouter", "o");
    work.sandbox = Some(rhema_config::SandboxSettings {
        kind: Some("docker".to_owned()),
        active: Some("work-box".to_owned()),
        ..rhema_config::SandboxSettings::default()
    });
    work.place_sandbox_instance(
        "work-box",
        rhema_config::SandboxInstance {
            kind: "docker".to_owned(),
            target: Some("work-box".to_owned()),
            vm: None,
        },
    );
    h.app.config.profiles.insert("work".to_owned(), work);
    let effects = h.app.switch_profile("work");
    assert!(h.app.sandboxed.is_some(), "the assembly stays");
    assert_eq!(h.app.assembling, None);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::AssembleSandbox(_))),
        "no re-assembly for the same place: {effects:?}"
    );
}

#[test]
fn the_offered_set_rederives_for_the_profile_moving_in() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // The ssh binary answers on this machine; personal names no
    // host, so ssh is not offered to it.
    h.app.sandbox_machines.ssh = true;
    h.app.sandbox_offered = offered(false, false, false, false, false);
    assert!(!h.app.sandbox_offered.ssh);
    let mut work = keyed_profile("openrouter", "o");
    work.sandbox = Some(rhema_config::SandboxSettings {
        host: Some("deploy@box".to_owned()),
        ..rhema_config::SandboxSettings::default()
    });
    h.app.config.profiles.insert("work".to_owned(), work);
    h.app.switch_profile("work");
    assert!(
        h.app.sandbox_offered.ssh,
        "work names a host: ssh is offered to it"
    );
}

#[test]
fn the_sandbox_badge_names_the_kind_or_the_instance() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert_eq!(h.app.sandbox_badge(), "local", "local is a fact too");
    h.app.sandboxed = Some(crate::sandbox::Sandboxed {
        choice: SandboxChoice::Docker,
        target: "work-box".to_owned(),
        tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
        workspace: "/workspace".to_owned(),
        skills: Vec::new(),
        instance_lock: None,
        managed: None,
        shared: None,
        session: Default::default(),
    });
    // No instance stored: the badge is the kind.
    assert_eq!(h.app.sandbox_badge(), "docker");
    // The active pointer names the instance the kind resolves to:
    // the badge says where, not just what.
    h.app.config.set_for_profile(Some("personal"), |profile| {
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
    assert_eq!(h.app.sandbox_badge(), "dev");
}

#[test]
fn profile_sandbox_reads_the_profile_alone() {
    // No stored kind reads local, even where the workspace
    // remembers another profile's docker: a switch never inherits.
    let everything = offered(true, true, false, true, true);
    assert_eq!(
        profile_sandbox(&rhema_config::Profile::default(), &everything),
        SandboxChoice::Local
    );
    // The stored kind wins while the machine offers it...
    let stored = sandbox_kind_profile("ssh");
    assert_eq!(profile_sandbox(&stored, &everything), SandboxChoice::Ssh);
    // ...and an unoffered kind falls to local, never to a memory.
    assert_eq!(
        profile_sandbox(&stored, &offered(false, false, false, false, false)),
        SandboxChoice::Local
    );
}

#[test]
fn slash_sandbox_opens_the_settings_sandbox_page() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(true, true, false, true, false);
    crate::commands::dispatch(&mut h.app, "/sandbox");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Sandbox);
    // An empty config: the host's own row and the add row, the
    // default view is only local.
    assert_eq!(
        settings
            .sandbox_rows()
            .iter()
            .map(|row| (row.name, row.offered))
            .collect::<Vec<_>>(),
        vec![("local", true), ("", false)]
    );
}

#[test]
fn enter_on_a_sandbox_instance_row_runs_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.sandbox_offered = offered(true, true, false, true, false);
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.place_sandbox_instance(
            "dev",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("rhema-dev".to_owned()),
                vm: None,
            },
        );
    });
    h.store.save(&h.app.config).unwrap();
    crate::commands::dispatch(&mut h.app, "/sandbox");
    // Down to the dev row, then enter: the assembly effect fires
    // and the page closes.
    h.app.on_key(key(KeyCode::Down));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::AssembleSandbox(SandboxChoice::Docker)],
        "enter on the dev instance assembles its kind"
    );
    assert!(matches!(h.app.screen, Screen::Chat), "enter returns");
    let stored = h.store.load().unwrap();
    let profile = stored.effective(Some("personal"));
    let sandbox = profile.sandbox.as_ref().expect("the section lands");
    assert_eq!(sandbox.kind.as_deref(), Some("docker"));
    assert_eq!(
        sandbox.active.as_deref(),
        Some("dev"),
        "the pointer names the instance"
    );
}

#[test]
fn sandbox_d_removes_the_instance_and_keeps_the_kind() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // A stored instance to remove, active and all.
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.place_sandbox_instance(
            "dev",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("rhema-dev".to_owned()),
                vm: None,
            },
        );
        profile.sandbox = Some(rhema_config::SandboxSettings {
            kind: Some("docker".to_owned()),
            active: Some("dev".to_owned()),
            container: Some("rhema-dev".to_owned()),
            ..rhema_config::SandboxSettings::default()
        });
    });
    h.store.save(&h.app.config).unwrap();
    crate::commands::dispatch(&mut h.app, "/sandbox");
    // One down to the dev row, then d.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Char('d')));
    let stored = h.store.load().unwrap();
    let profile = stored.effective(Some("personal"));
    assert!(
        !profile.sandbox_instances.contains_key("dev"),
        "the instance is gone from the map"
    );
    let sandbox = profile.sandbox.as_ref().expect("the section stays");
    assert_eq!(
        sandbox.active, None,
        "the active pointer went with the instance"
    );
    assert_eq!(
        sandbox.kind.as_deref(),
        Some("docker"),
        "the kind stays put"
    );
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("removed"))),
        "the notice says what happened"
    );
    // d on local does nothing: there is no instance there.
    h.app.on_key(key(KeyCode::Up));
    let notices = h.app.transcript.lines().len();
    h.app.on_key(key(KeyCode::Char('d')));
    assert_eq!(
        h.app.transcript.lines().len(),
        notices,
        "no notice: d on local is inert"
    );
}

#[test]
fn the_sandbox_add_dialog_stores_the_kinds_instance() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/sandbox");
    // One down: local, add row. Enter opens the dialog; enter
    // again stores docker, no name asked, the concrete one is
    // the workspace's to set at assembly time.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter)); // store docker
    let stored = h.store.load().unwrap();
    let profile = stored.effective(Some("personal"));
    let instance = profile
        .sandbox_instances
        .get("docker")
        .expect("the instance stored under the kind's name");
    assert_eq!(instance.kind, "docker");
    assert_eq!(
        instance.target, None,
        "no target collected: derived per workspace"
    );
    let sandbox = profile.sandbox.as_ref();
    assert!(
        sandbox
            .map(|s| s.kind.is_none() && s.active.is_none())
            .unwrap_or(true),
        "storing never activates: the kind and the pointer stay unset"
    );
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("still on the settings screen");
    };
    assert!(settings.sandbox_dialog.is_none(), "the dialog closed");
    assert!(
        h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::Notice(text) if text.contains("docker"))),
        "the notice names the instance"
    );
    // Same flow, lxd picked, two downs now, docker's row sits
    // between local and the add row. Its launch flag stays unset
    // (config.yaml's call).
    crate::commands::dispatch(&mut h.app, "/sandbox");
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter)); // store lxd
    let stored = h.store.load().unwrap();
    let profile = stored.effective(Some("personal"));
    let instance = profile
        .sandbox_instances
        .get("lxd")
        .expect("the lxd instance stored");
    assert_eq!(instance.kind, "lxd");
    assert_eq!(instance.target, None);
    assert_eq!(instance.vm, None, "the instance type stays config's");
}

#[test]
fn the_sandbox_palette_completes_the_instances() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.place_sandbox_instance(
            "dev",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("rhema-dev".to_owned()),
                vm: None,
            },
        );
        profile.place_sandbox_instance(
            "build",
            rhema_config::SandboxInstance {
                kind: "lxd".to_owned(),
                target: None,
                vm: Some(true),
            },
        );
    });
    let sandboxes = h.app.palette_sandboxes();
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
    // The empty argument lists the configured instances and
    // nothing else: the bare kinds never join the rows.
    let names: Vec<String> = rows("/sandbox ")
        .into_iter()
        .filter_map(|row| match row {
            crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
            _ => None,
        })
        .collect();
    // Map order: sandbox_instances is a BTreeMap, alphabetical.
    assert_eq!(names, ["build".to_owned(), "dev".to_owned()]);
    // A stem narrows across the name, the kind, and the target.
    assert_eq!(
        rows("/sandbox dev")
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["dev".to_owned()],
        "the instance name matches"
    );
    assert_eq!(
        rows("/sandbox rhema")
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["dev".to_owned()],
        "the target matches too"
    );
    // Tab fills the instance's name under the command prefix.
    let mut chat = crate::screens::ChatState::new();
    for c in "/sandbox dev".chars() {
        chat.handle_key(
            key(KeyCode::Char(c)),
            &[],
            &[],
            &sandboxes,
            &crate::icons::UNICODE,
        );
    }
    chat.handle_key(
        key(KeyCode::Tab),
        &[],
        &[],
        &sandboxes,
        &crate::icons::UNICODE,
    );
    assert_eq!(chat.input, "/sandbox dev");
}

#[test]
fn the_completion_cache_answers_the_same_query_without_a_rewalk() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.chat.input = "./src/m".to_owned();
    h.app.chat.cursor = h.app.chat.input.len();
    let models: Vec<crate::screens::CatalogModel> = Vec::new();
    let profiles: Vec<crate::screens::PaletteProfile> = Vec::new();
    let sandboxes: Vec<crate::screens::PaletteSandbox> = Vec::new();
    let first = crate::screens::palette_rows_cached(
        &mut h.app.chat,
        &models,
        &profiles,
        &sandboxes,
        h.app.icons,
    );
    assert!(!first.is_empty(), "the src dir completes");
    let cache = h.app.chat.completion.as_ref().unwrap();
    assert_eq!(cache.input, "./src/m");
    assert_eq!(
        cache.rows.len(),
        first.len(),
        "the cache holds what it built"
    );
    assert_eq!(format!("{:?}", cache.rows), format!("{:?}", first));
    // A changed query recomputes.
    h.app.chat.input = "./no-such-dir-xyz".to_owned();
    h.app.chat.cursor = h.app.chat.input.len();
    let second = crate::screens::palette_rows_cached(
        &mut h.app.chat,
        &models,
        &profiles,
        &sandboxes,
        h.app.icons,
    );
    assert!(second.is_empty(), "nothing completes under a missing dir");
    assert_eq!(
        h.app.chat.completion.as_ref().unwrap().input,
        "./no-such-dir-xyz"
    );
}

#[test]
fn slash_thinking_persists_the_level_and_rebuilds_the_agent() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/thinking high");
    assert_eq!(
        h.app.options.thinking,
        rhema_core::request::ThinkingEffort::High
    );
    let stored = h.store.load().unwrap();
    let profile = h.app.session_profile.clone().unwrap_or_default();
    assert_eq!(
        stored.effective(Some(&profile)).thinking.as_deref(),
        Some("high")
    );
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("thinking effort set to high".into()))
    );

    // An unknown level is declined in-band, and default stores its
    // name: the config never carries a bare null, which would read
    // as medium on the next load.
    crate::commands::dispatch(&mut h.app, "/thinking maximal");
    assert!(h.app.error.is_none());
    crate::commands::dispatch(&mut h.app, "/thinking default");
    assert_eq!(
        h.app.options.thinking,
        rhema_core::request::ThinkingEffort::Default
    );
    assert_eq!(
        h.store.load().unwrap().effective(Some(&profile)).thinking,
        Some("default".to_owned())
    );
}

#[test]
fn a_turn_records_its_profile_for_the_workspace() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let workspace = h.app.options.workspace.clone();
    h.app.session_profile = Some("work".to_owned());
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    h.app
        .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);
    assert_eq!(
        h.app.config.last_profile_in(&workspace).as_deref(),
        Some("work"),
        "the workspace remembers the profile"
    );
    // What the next launch reads is the saved file, not live state.
    assert_eq!(
        h.store
            .load()
            .unwrap()
            .last_profile_in(&workspace)
            .as_deref(),
        Some("work")
    );
}

#[test]
fn a_logged_in_profile_opens_the_chat_without_the_login_screen() {
    // The run_tui glue: resolve the no-flag profile, build the
    // backend from its section, and hand both to App::new. personal
    // is empty, work is ready with a model.
    let config = Config {
        settings: rhema_config::Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                (
                    "work".to_owned(),
                    rhema_config::Profile {
                        model: Some("work-model".into()),
                        ..keyed_profile("openrouter", "work-key")
                    },
                ),
            ]),
            ..rhema_config::Settings::default()
        },
        ..Default::default()
    };
    let profile = startup_profile(&config, Path::new("/nowhere"), None);
    let backend = Backend::from_profile(&config.effective(Some(&profile)), None);
    let app = App::new(AppSetup {
        store: Arc::new(MemoryStore::new(config.clone())),
        checkpoint: None,
        sessions_base: None,
        config,
        backend,
        session: Session::default(),
        secrets: None,
        sandboxed: None,
        sandbox_machines: crate::sandbox::SandboxMachines::default(),
        home: Some(std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id()))),
        onboard: false,
        options: RunOptions {
            colors: crate::palette::Palette::Rhema,
            theme: crate::palette::ThemeKind::Dark,
            background: crate::palette::Background::Rhema,
            m3_source: None,
            custom: None,
            thinking: rhema_core::request::ThinkingEffort::Default,
            icons: crate::icons::Flavor::Unicode,
            cursor: crate::cursor::CursorStyle::Default,
            scrollbar: true,
            profile: Some(profile),
            workspace: PathBuf::from("."),
            max_steps: NonZeroU32::new(4).unwrap(),
            no_transcript: false,
            base_system: None,
            save: None,
        },
    })
    .unwrap();
    assert!(matches!(app.screen, Screen::Chat), "no login screen");
    assert_eq!(app.session_profile.as_deref(), Some("work"));
    // The session reads work's section, so the footer shows its model.
    assert_eq!(app.effective().model.as_deref(), Some("work-model"));
}

#[test]
fn slash_settings_opens_the_live_providers_page() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Providers);
    assert_eq!(settings.profile, "personal");
    assert_eq!(settings.stored.provider_name().as_deref(), Some("google"));
    assert_eq!(settings.stored.model.as_deref(), Some("m-a"));
    let rows = settings.instance_rows();
    assert_eq!(rows.len(), 2, "the google instance plus the add row");
    assert!(!rows[0].add);
    assert!(rows[0].active, "the only instance is the serving one");
    assert!(rows[1].add);
    assert!(!settings.workspace.is_empty(), "the About page names it");
}

#[test]
fn slash_model_opens_the_models_page_of_the_settings() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/model");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Models);
    let rows = settings.model_picker.rows();
    assert_eq!(rows.len(), 2, "the cached catalog");
    // The cursor opens on the model the profile runs, not on the first
    // row: Enter twice in a row must not switch anything by accident.
    assert_eq!(rows[settings.model_picker.selected].id(), "m-a");
}

/// The Models page picks like `/model` always did: the choice persists,
/// the backend rebuilds, the page stays open, and its list marks what
/// runs.
#[test]
fn the_models_page_picks_and_persists() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    crate::commands::dispatch(&mut h.app, "/model");
    let key = |code| KeyEvent::new(code, KeyModifiers::empty());
    // Typing filters the list, not some other cursor.
    for c in "m-b".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.model_picker.row_count(), 1, "the filter narrowed");
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::FetchUsage], "the rebuild is inline");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("the pick keeps the page open");
    };
    assert_eq!(
        settings.stored.model.as_deref(),
        Some("m-b"),
        "the page follows"
    );
    assert!(settings.model_picker.filter.is_empty(), "the search resets");
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-b"),
        "a settings pick persists"
    );
    // The list names the model in use, and the cursor sits on it.
    let rows = settings.model_picker.rows();
    let selected = settings.model_picker.selected;
    assert_eq!(rows[selected].id(), "m-b", "the cursor follows the pick");
    assert_eq!(
        settings
            .model_picker
            .current
            .as_ref()
            .map(|c| c.id.as_str()),
        Some("m-b"),
        "the mark follows too"
    );

    // Esc leaves the page for the chat, the way every settings page does.
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
}

/// Ctrl-P is `/model` without the typing: the same page.
#[test]
fn ctrl_p_opens_the_models_page() {
    let mut h = harness(Some("g"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    h.app
        .on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Models);
}

#[test]
fn slash_subagents_opens_the_definitions_page() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/subagents");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Subagents);
    // The catalog is never empty: the built-in is always a row, with
    // no file to open until the first edit copies it out.
    let builtin = settings
        .subagents
        .iter()
        .find(|row| row.name == "general-purpose")
        .expect("the built-in row");
    assert_eq!(builtin.source, SubagentSource::Builtin);
    assert_eq!(builtin.path, None);
    assert!(
        !settings.subagents_workspace_dir.is_empty(),
        "the page names the workspace directory"
    );
}

#[test]
fn subagent_enter_on_a_definition_emits_the_edit_effect() {
    let workspace = tempfile::tempdir().unwrap();
    let folder = workspace
        .path()
        .join(".rhema")
        .join("subagents")
        .join("reviewer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SUBAGENT.md"),
        "---\ndescription: reviews diffs\n---\n\nReview the diff.\n",
    )
    .unwrap();
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.options.workspace = workspace.path().to_path_buf();
    crate::commands::dispatch(&mut h.app, "/subagents");
    // The rows sort by name: the built-in first, the definition
    // second. The path is discovery's own, the canonicalized
    // workspace, since the opener canonicalizes.
    let file = std::fs::canonicalize(workspace.path())
        .unwrap()
        .join(".rhema")
        .join("subagents")
        .join("reviewer")
        .join("SUBAGENT.md");
    h.app.on_key(key(KeyCode::Down));
    assert_eq!(
        h.app.on_key(key(KeyCode::Enter)),
        vec![Effect::EditSubagent(file)]
    );
}

#[test]
fn a_refresh_after_the_editor_picks_up_what_it_changed() {
    let workspace = tempfile::tempdir().unwrap();
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.options.workspace = workspace.path().to_path_buf();
    crate::commands::dispatch(&mut h.app, "/subagents");
    // The edit happens in $EDITOR, behind the app's back...
    let folder = workspace
        .path()
        .join(".rhema")
        .join("subagents")
        .join("reviewer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SUBAGENT.md"),
        "---\ndescription: reviews diffs\n---\n\nReview the diff.\n",
    )
    .unwrap();
    // ...and the runner asks for the rows back once the TUI
    // re-enters.
    h.app.refresh_subagents();
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    let reviewer = settings
        .subagents
        .iter()
        .find(|row| row.name == "reviewer")
        .expect("the definition written behind the app's back");
    assert_eq!(reviewer.source, SubagentSource::Workspace);
    assert!(reviewer.path.is_some(), "the row names the file to edit");
}

#[test]
fn subagent_rows_carry_the_provenance_discovery_shadows_by() {
    let global = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let global_folder = global.path().join("reviewer");
    std::fs::create_dir_all(&global_folder).unwrap();
    std::fs::write(
        global_folder.join("SUBAGENT.md"),
        "---\ndescription: global trigger\n---\n\nBody.\n",
    )
    .unwrap();
    let workspace_folder = workspace
        .path()
        .join(".rhema")
        .join("subagents")
        .join("reviewer");
    std::fs::create_dir_all(&workspace_folder).unwrap();
    std::fs::write(
        workspace_folder.join("SUBAGENT.md"),
        "---\ndescription: workspace trigger\n---\n\nBody.\n",
    )
    .unwrap();

    let disabled = Default::default();
    let rows = subagent_rows(Some(global.path()), workspace.path(), &disabled);
    let reviewer = rows
        .iter()
        .find(|row| row.name == "reviewer")
        .expect("the shadowed definition");
    assert_eq!(
        reviewer.source,
        SubagentSource::Workspace,
        "the workspace shadows the global one"
    );
    assert_eq!(reviewer.description, "workspace trigger");
    assert_eq!(
        reviewer.path.as_deref(),
        Some(workspace_folder.join("SUBAGENT.md").as_path())
    );

    // Without the workspace file the same name reads as global.
    std::fs::remove_dir_all(&workspace_folder).unwrap();
    let rows = subagent_rows(Some(global.path()), workspace.path(), &Default::default());
    let reviewer = rows.iter().find(|row| row.name == "reviewer").unwrap();
    assert_eq!(reviewer.source, SubagentSource::Global);
    assert_eq!(
        reviewer.path.as_deref(),
        Some(global_folder.join("SUBAGENT.md").as_path())
    );
}

#[test]
fn subagent_rows_carry_the_disabled_set_as_the_enabled_flag() {
    let global = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let folder = global.path().join("reviewer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SUBAGENT.md"),
        "---\ndescription: reviews diffs\n---\n\nBody.\n",
    )
    .unwrap();

    // The disabled set inverts into the flag: off rows still list.
    let disabled = std::collections::BTreeSet::from(["reviewer".to_owned()]);
    let rows = subagent_rows(Some(global.path()), workspace.path(), &disabled);
    assert_eq!(rows.len(), 2, "the built-in and the off reviewer");
    let reviewer = rows.iter().find(|row| row.name == "reviewer").unwrap();
    assert!(!reviewer.enabled);
    let builtin = rows
        .iter()
        .find(|row| row.name == "general-purpose")
        .expect("the built-in lists beside the disabled row");
    assert!(builtin.enabled);

    // An empty set enables everything.
    let rows = subagent_rows(Some(global.path()), workspace.path(), &Default::default());
    assert!(rows.iter().all(|row| row.enabled));
}

#[test]
fn shadowing_the_builtin_writes_a_discoverable_definition() {
    let global = tempfile::tempdir().unwrap();
    let file = shadow_builtin(global.path(), "general-purpose").unwrap();
    assert_eq!(
        file,
        global.path().join("general-purpose").join("SUBAGENT.md")
    );
    // What the copy wrote is a definition discovery accepts, the
    // shadow replaces the built-in wholesale, description and body.
    let workspace = tempfile::tempdir().unwrap();
    let defs = rhema_subagents::discover(Some(global.path()), workspace.path());
    assert_eq!(defs.len(), 1, "the shadow leaves nothing of the built-in");
    assert_eq!(defs[0].description, rhema_subagents::builtin().description);
    assert_eq!(defs[0].body, rhema_subagents::builtin().body);
    // An existing file is left alone: it is a shadow already.
    std::fs::write(&file, "---\ndescription: mine\n---\n\nMine.\n").unwrap();
    assert_eq!(
        shadow_builtin(global.path(), "general-purpose").unwrap(),
        file
    );
    let defs = rhema_subagents::discover(Some(global.path()), workspace.path());
    assert_eq!(defs[0].description, "mine");
}

#[test]
fn settings_enter_on_the_add_row_opens_the_dialog() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    // Down to the add row, enter: the pop-in dialog comes up.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.dialog.is_some());
    // Esc closes the dialog, then leaves the settings for the chat.
    h.app.on_key(key(KeyCode::Esc));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.dialog.is_none());
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn settings_enter_on_an_instance_reactivates_it_and_lands_in_the_chat() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    // Enter on the highlighted instance re-activates it; with the
    // model stored, the session lands back in the chat.
    h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Chat));
}

/// `/settings` opens over a running turn without steering it: the
/// model keeps working in the background, the screens read
/// snapshots, and esc hands the still-running turn back.
#[test]
fn slash_settings_opens_over_a_running_turn_without_steering() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    for c in "/settings".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Settings(_)));
    assert!(h.app.turn.running);
    assert!(effects.is_empty(), "no steer, no turn start: {effects:?}");
    // The command never reached the transcript as a prompt.
    assert!(
        !h.app
            .transcript
            .lines()
            .iter()
            .any(|line| matches!(line, Line::User { .. }))
    );
    // Esc leaves the screen; the turn is still running underneath.
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(h.app.turn.running);
}

/// Every other command keeps its idle-only rule while a turn runs:
/// the input is kept, nothing dispatches, nothing steers.
#[test]
fn other_commands_stay_inert_while_a_turn_runs() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    for c in "/model".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(h.app.chat.input, "/model");
}

#[test]
fn settings_instance_activation_waits_for_a_running_turn() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    h.app.turn.running = true;
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(h.app.screen, Screen::Settings(_)),
        "the switch waits for an idle session"
    );
    // The screen itself still answers: esc leaves.
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn settings_toggles_the_theme_live_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert_eq!(h.app.theme, crate::palette::DARK);
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    h.app.on_key(key(KeyCode::Char('t')));
    assert_eq!(h.app.theme, crate::palette::LIGHT, "the swap is live");
    assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
    // And back.
    h.app.on_key(key(KeyCode::Char('t')));
    assert_eq!(h.app.theme, crate::palette::DARK);
    assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("dark"));
}

#[test]
fn settings_toggles_the_icon_flavor_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    h.app.on_key(key(KeyCode::Char('i')));
    assert_eq!(h.app.options.icons, crate::icons::Flavor::Nerd);
    assert!(
        std::ptr::eq(h.app.icons, crate::icons::Flavor::Nerd.set()),
        "the swap is live"
    );
    assert_eq!(h.store.load().unwrap().icons.as_deref(), Some("nerd"));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.icons_nerd, "the page follows the toggle");
}

#[test]
fn export_skips_a_session_with_nothing_to_resume() {
    // A fresh session saves nothing: there is no conversation.
    let h = harness(None, None, Session::default());
    assert_eq!(h.app.export_session(), None);

    // A session of notices alone (a switch, a decline) saves nothing
    // either: the history, not the transcript, decides.
    let mut h = harness(None, None, Session::default());
    h.app.push_notice("switched to profile work");
    assert_eq!(h.app.export_session(), None);

    // One prompt in: the live session exports, with no origin file.
    let mut h = harness(Some("k"), None, Session::default());
    h.app
        .session
        .history
        .push(Message::text(Role::User, "fix it"));
    let (exported, origin) = h.app.export_session().expect("a spoken session exports");
    assert_eq!(exported.history.len(), 1);
    assert_eq!(origin, None);
}

#[test]
fn slash_resume_opens_a_picker_and_picking_resumes() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // Two saved sessions for this workspace, newest first.
    let base = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(&h.app.options.workspace).unwrap();
    let older =
        crate::sessions::save_session(base.path(), &workspace, &session_of("the older prompt"))
            .unwrap()
            .unwrap();
    let newer =
        crate::sessions::save_session(base.path(), &workspace, &session_of("the newer prompt"))
            .unwrap()
            .unwrap();
    // Speak one line first: resuming replaces it.
    h.app.transcript.push_user("current talk", Vec::new());

    // The picker over the temp base; the dispatch path resolves its
    // own base from the environment, which a test cannot own.
    h.app.open_resume_from(base.path());
    match &h.app.screen {
        Screen::Resume(resume) => {
            assert_eq!(resume.entries.len(), 2);
            assert_eq!(resume.entries[0].path, newer, "newest first");
            assert_eq!(
                resume.entries[0].snippet.as_deref(),
                Some("the newer prompt")
            );
        }
        _other => panic!("expected the resume picker"),
    }

    // Down moves to the older session; enter requests the load, the
    // screen holds until the parts come back off the blocking pool.
    h.app.on_key(key(KeyCode::Down));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(effects.as_slice(), [Effect::ResumeSession(_)]));
    assert!(matches!(h.app.screen, Screen::Resume(_)));
    let path = older.clone();
    let loaded = crate::app::App::load_resume_parts(&path).unwrap();
    h.app.on_event(UiEvent::SessionResumed {
        path,
        result: Ok(Box::new(*loaded)),
    });
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(
        h.app
            .session
            .history
            .first()
            .and_then(|m| match m.content.first() {
                Some(rhema_core::Content::Text(text)) => Some(text.as_str()),
                _ => None,
            }),
        Some("the older prompt"),
        "the older session is the live one"
    );
    assert!(h.app.agent.is_some());
    assert_eq!(
        h.app.transcript.lines().first(),
        Some(&Line::User {
            pastes: Vec::new(),
            text: "the older prompt".into(),
            images: Vec::new()
        })
    );
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.starts_with("resumed ")
    ));
    // The session is now bound to the file it came from: exiting
    // updates it in place instead of listing it twice.
    assert_eq!(h.app.session_file.as_deref(), Some(older.as_path()));
    let (exported, origin) = h.app.export_session().expect("a spoken session exports");
    assert_eq!(origin.as_deref(), Some(older.as_path()));
    assert_eq!(exported.history.len(), 1);
    // The file the picker named is what got loaded.
    assert!(older.is_file() && newer.is_file());
}

#[test]
fn the_startup_checkpoint_names_the_session_file() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let checkpoint =
        std::env::temp_dir().join(format!("rhema-checkpoint-{}-a.json", std::process::id()));
    h.app.session_file = Some(checkpoint.clone());

    // A fresh run's turns checkpoint into the allocated file.
    assert_eq!(h.app.checkpoint_target(), Some(checkpoint.clone()));
    // An explicit --save outranks it, for turns and for the exit
    // save alike.
    h.app.options.save = Some(PathBuf::from("/tmp/explicit-save.json"));
    assert_eq!(
        h.app.checkpoint_target(),
        Some(PathBuf::from("/tmp/explicit-save.json"))
    );
}

#[test]
fn clear_drops_the_session_file_so_the_next_conversation_owns_its_own() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.session_file = Some(PathBuf::from("/tmp/cleared.json"));
    crate::commands::dispatch(&mut h.app, "/clear");
    assert_eq!(h.app.session_file, None);
    assert_eq!(h.app.checkpoint_target(), None);
}

#[test]
fn slash_resume_with_nothing_saved_is_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let base = tempfile::tempdir().unwrap();
    h.app.open_resume_from(base.path());
    assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("no saved sessions")
    ));
}

fn session_of(prompt: &str) -> Session {
    let mut session = Session::default();
    session.history.push(Message::text(Role::User, prompt));
    session
}

#[test]
fn slash_new_backs_up_the_conversation_then_starts_a_fresh_one() {
    let mut h = harness(Some("k"), Some("m-a"), session_of("talk"));
    let old = h.sessions.path().join("old.json");
    h.app.session_file = Some(old.clone());

    crate::commands::dispatch(&mut h.app, "/new");

    // The outgoing conversation landed before the wipe, in the file its
    // turns checkpointed into, now holding the final state.
    let saved = HostAgent::load_session_from(&old).expect("the old conversation was written");
    assert_eq!(saved.history.len(), 1);

    // A fresh session: no history to save, no resume binding, the
    // transcript down to the notice.
    assert_eq!(h.app.export_session(), None);
    assert_eq!(h.app.session_file, None);
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("starting fresh")
    ));
}

#[test]
fn new_and_clear_are_one_command() {
    let row = |name: &str| {
        crate::commands::COMMANDS
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("no {name} row"))
    };
    // Two palette entries, one behavior: the same handler, word, and
    // argument rule.
    assert!(std::ptr::fn_addr_eq(
        row("/new").handler,
        row("/clear").handler
    ));
    assert_eq!(row("/new").description, row("/clear").description);
    assert_eq!(row("/new").argument, row("/clear").argument);
}

#[test]
fn resume_and_r_are_one_command() {
    let row = |name: &str| {
        crate::commands::COMMANDS
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("no {name} row"))
    };
    // Two palette entries, one behavior: the same handler, word, and
    // argument rule.
    assert!(std::ptr::fn_addr_eq(
        row("/r").handler,
        row("/resume").handler
    ));
    assert_eq!(row("/r").description, row("/resume").description);
    assert_eq!(row("/r").argument, row("/resume").argument);
    // `/r` resolves to its own row, not to `/retry`, the row it shares
    // a first letter with.
    assert!(
        matches!(
            crate::commands::parse("/r"),
            crate::commands::Parsed::Command { spec, argument: None }
                if spec.name == "/r"
        ),
        "/r must parse as the resume alias"
    );
}

#[test]
fn slash_clear_backs_up_a_conversation_that_had_no_file() {
    let mut h = harness(Some("k"), Some("m-a"), session_of("unfiled"));
    assert_eq!(h.app.session_file, None);

    crate::commands::dispatch(&mut h.app, "/clear");

    // The conversation had no binding, so the clear allocated one in
    // the sessions cache before wiping the history.
    let workspace = crate::args::canonical(&h.app.options.workspace);
    let rows = crate::sessions::list_sessions(h.sessions.path(), &workspace).unwrap();
    assert_eq!(rows.len(), 1);
    let saved = HostAgent::load_session_from(&rows[0].path).expect("the talk was written");
    assert_eq!(saved.history.len(), 1);
    assert_eq!(h.app.session_file, None);
    assert_eq!(h.app.export_session(), None);
}

#[test]
fn a_clear_keeps_the_backup_out_of_the_explicit_save_file() {
    let mut h = harness(Some("k"), Some("m-a"), session_of("first talk"));
    let own = h.sessions.path().join("own.json");
    let explicit = h.sessions.path().join("explicit.json");
    h.app.session_file = Some(own.clone());
    h.app.options.save = Some(explicit.clone());

    crate::commands::dispatch(&mut h.app, "/clear");

    // The outgoing conversation landed in its own file, so the run's
    // explicit checkpoint path stays free for the conversation that
    // follows instead of being overwritten by it.
    let saved = HostAgent::load_session_from(&own).expect("the talk was written");
    assert_eq!(saved.history.len(), 1);
    assert!(!explicit.exists());
}

#[test]
fn a_failed_backup_does_not_block_the_clear() {
    let mut h = harness(Some("k"), Some("m-a"), session_of("cannot land"));
    // A directory is not a writable session file.
    let dir = h.sessions.path().join("not-a-file");
    std::fs::create_dir_all(&dir).unwrap();
    h.app.session_file = Some(dir);

    crate::commands::dispatch(&mut h.app, "/clear");

    assert_eq!(h.app.export_session(), None);
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("was not saved")
    ));
}

#[test]
fn a_cleared_conversation_gets_a_fresh_checkpoint_file_on_its_next_turn() {
    let mut h = harness(Some("k"), Some("m-a"), session_of("first"));
    let first = h.sessions.path().join("first.json");
    h.app.session_file = Some(first.clone());
    crate::commands::dispatch(&mut h.app, "/clear");
    assert_eq!(h.app.session_file, None, "the backup keeps its own file");

    // The next turn allocates a file of its own instead of overwriting
    // the backup.
    let next = h.app.ensure_checkpoint_target().expect("a fresh file");
    assert_ne!(next, first);
    assert_eq!(h.app.session_file, Some(next.clone()));
    assert_eq!(h.app.checkpoint_target(), Some(next.clone()));
    // A second call keeps the conversation's file: no per-turn growth.
    assert_eq!(h.app.ensure_checkpoint_target(), Some(next));
}

#[test]
fn slash_skills_opens_the_skills_page() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/skills");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Skills);
    // Esc returns to the chat like every other settings page.
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn slash_system_shows_the_prompt_in_a_fenced_block() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());

    crate::commands::dispatch(&mut h.app, "/system");

    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Markdown(text))
            if text.starts_with("```markdown\n") && text.ends_with("```")
    ));
    // An argument is declined like any no-argument command.
    crate::commands::dispatch(&mut h.app, "/system now");
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("unknown")
    ));
}

#[test]
fn model_choice_lands_in_the_session_profile() {
    // Started with no --profile (so personal), then switched to work:
    // the pick must follow the session, not the startup flag.
    let mut h = harness(Some("k"), None, Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), keyed_profile("openrouter", "work-key"));
    crate::commands::dispatch(&mut h.app, "/profile work");
    crate::commands::dispatch(&mut h.app, "/model");
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: loaded("google", models()),
    });
    // Enter picks the first row (the picker sorts by id): "m-a".
    h.app.on_key(key(KeyCode::Enter));
    // The pick persists into work's section; personal's stays empty.
    let stored = h.store.load().unwrap();
    assert_eq!(stored.effective(Some("work")).model.as_deref(), Some("m-a"));
    assert_eq!(stored.effective(Some("personal")).model, None);
    // The rebuilt session reads the same section back.
    assert_eq!(h.app.effective().model.as_deref(), Some("m-a"));
}

#[test]
fn slash_profile_without_credentials_still_switches() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.config.profiles.insert(
        "work".to_owned(),
        rhema_config::Profile {
            provider: Some("openrouter".into()),
            ..rhema_config::Profile::default()
        },
    );
    crate::commands::dispatch(&mut h.app, "/profile work");
    // The identity moves even without a key...
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("no API key")
    ));
    // ...but the old provider goes with the old profile: no backend,
    // no agent, no model list.
    assert!(h.app.backend.is_none());
    assert!(h.app.provider.is_none());
    assert!(h.app.agent.is_none());
    assert_eq!(h.app.effective().model, None, "reads work's section");

    // A prompt on a keyless profile is declined in-band and the text
    // stays for later.
    h.app.chat.input = "hello".to_owned();
    h.app.chat.cursor = 5;
    assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(h.app.chat.input == "hello", "the prompt is kept");
    assert!(!h.app.turn.running, "no fake turn starts");
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("/login")
    ));

    // /model is declined too: there is no backend to fetch from.
    crate::commands::dispatch(&mut h.app, "/model");
    assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");

    // And the way back rebuilds on the kept session.
    crate::commands::dispatch(&mut h.app, "/profile personal");
    assert!(h.app.backend.is_some());
    assert!(h.app.agent.is_some());
    assert_eq!(h.app.session_profile.as_deref(), Some("personal"));
}

#[test]
fn slash_profile_alone_opens_the_picker() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/profile");
    match &h.app.screen {
        Screen::Profile(profile) => {
            assert_eq!(profile.entries.len(), 1);
            assert_eq!(profile.entries[0].name, "personal");
        }
        _ => panic!("expected the profile picker"),
    }
}

#[test]
fn slash_model_opens_the_models_page_and_esc_returns_to_the_chat() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/model");
    assert!(matches!(
        &h.app.screen,
        Screen::Settings(settings) if settings.section == SettingsSection::Models
    ));
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
}

#[test]
fn slash_login_opens_the_providers_page_and_esc_returns_without_quitting() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/login");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Providers);
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(!h.app.should_quit, "esc goes back, not out");
}

#[test]
fn esc_on_the_wizard_s_welcome_quits() {
    let mut h = harness(None, None, Session::default());
    h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
        Vec::new(),
        "personal".into(),
        h.app.config.profiles.clone(),
        false,
        rhema_config::Profile::default(),
    )));
    h.app.on_key(key(KeyCode::Esc));
    assert!(h.app.should_quit, "no config behind the wizard: esc quits");
    // Mid-wizard, Esc walks back instead: the finish step is the
    // only one whose back-move cannot pass the welcome step.
    let mut h = harness(None, None, Session::default());
    h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
        Vec::new(),
        "personal".into(),
        h.app.config.profiles.clone(),
        false,
        rhema_config::Profile::default(),
    )));
    h.app.on_key(key(KeyCode::Enter)); // welcome -> login
    h.app.on_key(key(KeyCode::Esc));
    assert!(
        !h.app.should_quit,
        "an abandoned wizard only ever marks itself unfinished"
    );
    assert!(h.app.config.onboarded != Some(true));
}

/// A wizard fresh from `onboard: true` in the setup.
fn wizard_harness() -> Harness {
    let mut h = harness(None, None, Session::default());
    h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
        Vec::new(),
        "personal".into(),
        h.app.config.profiles.clone(),
        false,
        h.app.effective(),
    )));
    h
}

#[test]
fn the_wizard_walks_skip_to_finish_and_marks_the_config_onboarded() {
    let mut h = wizard_harness();
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Welcome);

    // Welcome -> Profile; keep personal -> Login; skip the login
    // (row 1), landing on Colors.
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Login);
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Appearance);

    // Colors -> Skills -> Finish -> chat.
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(h.app.screen, Screen::Chat));
    assert_eq!(h.app.config.onboarded, Some(true));
    let stored = h.store.load().unwrap();
    assert_eq!(stored.onboarded, Some(true));
}

#[test]
fn the_wizard_s_profile_step_makes_an_existing_profile_the_default() {
    let mut h = wizard_harness();
    h.app
        .config
        .profiles
        .insert("work".to_owned(), rhema_config::Profile::default());
    if let Screen::Onboarding(wizard) = &mut h.app.screen {
        wizard.profiles = h.app.config.profiles.clone();
    }
    h.app.on_key(key(KeyCode::Enter)); // welcome -> profile
    h.app.on_key(key(KeyCode::Down)); // personal -> work
    h.app.on_key(key(KeyCode::Enter));
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Login);
    assert_eq!(wizard.default_profile, "work");
    assert_eq!(
        h.store.load().unwrap().last_profile_in(&PathBuf::from(".")),
        Some("work".to_owned()),
        "the choice is the workspace's memory"
    );
}

#[test]
fn the_wizard_s_add_row_creates_and_selects_a_profile() {
    let mut h = wizard_harness();
    h.app.on_key(key(KeyCode::Enter)); // welcome -> profile
    h.app.on_key(key(KeyCode::Down)); // personal -> the add row
    h.app.on_key(key(KeyCode::Enter)); // opens the name input
    for c in "home".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert_eq!(h.app.session_profile.as_deref(), Some("home"));
    assert!(
        h.app.config.profiles.contains_key("home"),
        "the registry holds the new profile"
    );
    assert!(h.store.load().unwrap().profiles.contains_key("home"));
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Login);
    assert_eq!(wizard.default_profile, "home");
    assert!(wizard.dialog.is_none(), "the login rows are up");
}

#[test]
fn the_wizard_s_login_advances_to_its_own_model_step() {
    let mut h = wizard_harness();
    h.app.on_key(key(KeyCode::Enter)); // welcome -> profile
    h.app.on_key(key(KeyCode::Enter)); // keep personal -> login
    h.app.on_key(key(KeyCode::Enter)); // add a login -> the dialog
    // Down to API, then the first provider (Google), the suggested
    // name, a key.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Char('k')));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(
        effects,
        vec![Effect::FetchModels(FetchWhy::Login {
            backend: Backend::Google {
                api_key: "k".into(),
            },
            instance: Some("Google Gemini".into()),
        })],
        "the wizard validates exactly like the settings dialog"
    );
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Login {
            backend: Backend::Google {
                api_key: "k".into(),
            },
            instance: Some("Google Gemini".into()),
        },
        result: loaded("google", models()),
        remaining: 0,
    });
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Model);
    assert_eq!(wizard.picker.models.len(), 2);
    // Pick the highlighted model: the agent builds and the step
    // lands on Colors.
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::FetchUsage]);
    assert!(h.app.agent.is_some());
    assert_eq!(
        h.store.load().unwrap().effective(None).model.as_deref(),
        Some("m-a")
    );
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Appearance);
    assert_eq!(wizard.chosen_model.as_deref(), Some("m-a"));
}

#[test]
fn the_wizard_s_colors_step_previews_and_the_settings_skills_page_toggles() {
    let mut h = wizard_harness();
    // Walk to Colors and step one palette down: the live apply is
    // the app's business, and the config is its record.
    h.app.on_key(key(KeyCode::Enter)); // welcome -> profile
    h.app.on_key(key(KeyCode::Enter)); // keep personal -> login
    h.app.on_key(key(KeyCode::Down)); // the skip row
    h.app.on_key(key(KeyCode::Enter)); // -> colors
    h.app.on_key(key(KeyCode::Down));
    let expected = crate::palette::Palette::ALL[1];
    assert_eq!(h.app.options.colors, expected, "applied live");
    assert_eq!(
        h.app.config.colors.as_deref(),
        Some(expected.stored_name()),
        "and persisted"
    );
    assert_eq!(
        h.store.load().unwrap().colors,
        h.app.config.colors,
        "and persisted"
    );

    // A skills toggle from the settings page lands in the disabled
    // set and flips the row on screen.
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.config.skills_disabled.clear();
    h.app
        .open_settings_at(crate::screens::SettingsSection::Skills);
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.skills = vec![crate::screens::SkillRow {
            name: "unslop".into(),
            description: "trigger".into(),
            license: Some("MIT".into()),
            source: crate::screens::SkillSource::Bundled,
            enabled: true,
        }];
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        h.app.config.skills_disabled.contains("unslop"),
        "the toggle lands in the disabled set"
    );
    assert_eq!(
        h.store.load().unwrap().skills_disabled,
        h.app.config.skills_disabled
    );
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(!settings.skills[0].enabled, "the row follows");
    }
    // Toggle back on: the set clears (and a bundled skill would
    // re-seed from the re-enable path).
    h.app.on_key(key(KeyCode::Enter));
    assert!(!h.app.config.skills_disabled.contains("unslop"));
}

#[test]
fn the_subagents_page_toggles_through_space_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Subagents);
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.subagents = vec![crate::screens::SubagentRow {
            enabled: true,
            name: "reviewer".into(),
            description: "reviews diffs".into(),
            model: None,
            max_steps: None,
            source: crate::screens::SubagentSource::Workspace,
            path: Some("/w/.rhema/subagents/reviewer/SUBAGENT.md".into()),
        }];
    }
    // Space (not enter, that opens $EDITOR) flips the row into the
    // disabled set and persists it.
    h.app.on_key(key(KeyCode::Char(' ')));
    assert!(
        h.app.config.disabled_subagents.contains("reviewer"),
        "the toggle lands in the disabled set"
    );
    assert_eq!(
        h.store.load().unwrap().disabled_subagents,
        h.app.config.disabled_subagents
    );
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(!settings.subagents[0].enabled, "the row follows");
    }
    // Space again turns it back on.
    h.app.on_key(key(KeyCode::Char(' ')));
    assert!(!h.app.config.disabled_subagents.contains("reviewer"));
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(settings.subagents[0].enabled, "the row follows back");
    }
}

#[test]
fn the_profiles_page_adds_a_profile_through_its_dialog() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Profiles);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert_eq!(settings.profiles.len(), 1, "the harness ships personal");
    // The add row is the row past the profiles.
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.row = settings.profiles.len();
    }
    h.app.on_key(key(KeyCode::Enter));
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(settings.add_profile.is_some(), "the dialog opened");
    }
    for c in "side".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(h.app.config.profiles.contains_key("side"), "created");
    assert_eq!(
        h.app.config.profiles["side"],
        rhema_config::Profile::default(),
        "creation is not a switch: the profile stays empty"
    );
    assert!(
        h.store.load().unwrap().profiles.contains_key("side"),
        "and persisted"
    );
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert!(settings.add_profile.is_none(), "the dialog closed");
    assert_eq!(settings.profiles.len(), 2, "the snapshot follows");

    // A taken name refuses in the dialog and changes nothing.
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.row = settings.profiles.len();
    }
    h.app.on_key(key(KeyCode::Enter));
    for c in "side".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert!(settings.add_profile.is_some(), "the dialog stayed up");
    assert!(
        settings.add_profile.as_ref().unwrap().error.is_some(),
        "the taken name names the error"
    );
    assert_eq!(h.app.config.profiles.len(), 2, "nothing was overwritten");
}

#[test]
fn d_removes_a_profile_the_session_is_not_on() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), rhema_config::Profile::default());
    h.app
        .config
        .last_profile_by_workspace
        .insert("/proj".to_owned(), "work".to_owned());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Profiles);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert_eq!(settings.profiles.len(), 2, "personal and work, sorted");
    // Row 1 is work; the session runs personal.
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.row = 1;
    }
    // d arms the confirm; the profile still holds the row.
    h.app.on_key(key(KeyCode::Char('d')));
    {
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.profile_confirm.as_deref(), Some("work"));
    }
    assert!(
        h.app.config.profiles.contains_key("work"),
        "d alone removes nothing"
    );
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(!h.app.config.profiles.contains_key("work"));
    let stored = h.store.load().unwrap();
    assert!(stored.profiles.contains_key("personal"));
    assert!(!stored.profiles.contains_key("work"), "and persisted");
    assert!(
        !stored
            .last_profile_by_workspace
            .values()
            .any(|remembered| remembered == "work"),
        "the workspace memory forgets the name"
    );
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert_eq!(settings.profiles.len(), 1, "the snapshot follows");
    assert!(settings.row <= settings.profiles.len(), "inside the list");
}

#[test]
fn the_active_profile_refuses_removal() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), rhema_config::Profile::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Profiles);
    // Row 0 is personal, the profile this session runs on. Its
    // removal is also what keeps the registry from emptying.
    h.app.on_key(key(KeyCode::Char('d')));
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(
        h.app.config.profiles.contains_key("personal"),
        "the session's profile stays"
    );
    assert!(
        h.store.load().unwrap().profiles.contains_key("personal"),
        "and the stored config never loses it"
    );
}

#[test]
fn c_cycles_a_profile_color_through_the_wheel_and_persists() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Profiles);
    // Row 0 is personal.
    h.app.on_key(key(KeyCode::Char('c')));
    assert_eq!(
        h.app.config.profiles["personal"].color.as_deref(),
        Some(crate::palette::PROFILE_COLORS[0]),
        "auto steps to the first wheel color"
    );
    assert_eq!(
        h.store.load().unwrap().profiles["personal"].color,
        h.app.config.profiles["personal"].color,
        "and persists"
    );
    for _ in 0..crate::palette::PROFILE_COLORS.len() {
        h.app.on_key(key(KeyCode::Char('c')));
    }
    assert_eq!(
        h.app.config.profiles["personal"].color, None,
        "the wheel wraps to auto"
    );
    // A hand-edited color the wheel does not spell rejoins at auto.
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.color = Some("#12abCD".to_owned());
    });
    h.app.on_key(key(KeyCode::Char('c')));
    assert_eq!(h.app.config.profiles["personal"].color, None);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected settings");
    };
    assert_eq!(settings.profiles[0].color, None, "the swatch follows");
}

#[test]
fn enter_on_a_profiles_row_switches_the_session() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .config
        .profiles
        .insert("work".to_owned(), rhema_config::Profile::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Profiles);
    if let Screen::Settings(settings) = &mut h.app.screen {
        settings.row = 1; // work, which holds no credentials
    }
    h.app.on_key(key(KeyCode::Enter));
    assert_eq!(h.app.session_profile.as_deref(), Some("work"));
    assert!(
        matches!(h.app.screen, Screen::Chat),
        "the switch lands back in the chat"
    );
    assert!(
        h.app.backend.is_none(),
        "work holds no key: the backend waits for /login"
    );
}

#[test]
fn a_click_on_the_wizard_s_rail_jumps_to_the_step() {
    let mut h = wizard_harness();
    // Draw one frame so the renderer registers the rail rects,
    // then click the Finish row (the rail's last step).
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let (row_x, row_y) = h
        .app
        .onboarding_hits
        .iter()
        .find_map(|(area, hit)| {
            matches!(
                hit,
                crate::app::OnboardingHit::RailStep(crate::onboarding::OnboardingStep::Finish)
            )
            .then_some((area.x, area.y))
        })
        .expect("the finish row is clickable");
    h.app.on_click(row_x, row_y);
    let Screen::Onboarding(wizard) = &h.app.screen else {
        panic!("expected the wizard");
    };
    assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Finish);
    // A click lands on the row's cell, not beside it: one off the
    // rect changes nothing.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    h.app.on_click(row_x + 40, row_y);
    if let Screen::Onboarding(wizard) = &h.app.screen {
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Finish);
    }
}

#[test]
fn a_rerun_shows_the_existing_configuration() {
    // A configured profile walks in: the welcome rail says what is
    // connected instead of starting from a blank slate.
    let mut h = wizard_harness();
    if let Screen::Onboarding(wizard) = &mut h.app.screen {
        let mut stored = rhema_config::Profile {
            provider: Some("google".into()),
            instance: Some("google".into()),
            model: Some("gemini-x".into()),
            ..rhema_config::Profile::default()
        };
        stored.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: None,
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        wizard.stored = stored;
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let text: String = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| {
                    let cell = &terminal.backend().buffer()[(x, y)];
                    cell.symbol().to_owned()
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Currently configured"),
        "the welcome step names the config: {text}"
    );
    assert!(text.contains("google (google)"), "{text}");
    assert!(text.contains("gemini-x"), "{text}");

    // The login step lists the held instance; the model step keeps
    // the stored model.
    h.app.on_click(0, 0); // no-op; walk with keys instead
    if let Screen::Onboarding(wizard) = &mut h.app.screen {
        wizard.go(crate::onboarding::OnboardingStep::Login);
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let text: String = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("google (google)"), "login lists it: {text}");
    assert!(text.contains("+ add a login"), "{text}");

    // The finish summary names the stored provider and model too: a
    // rerun that skipped the login is not told "none yet".
    if let Screen::Onboarding(wizard) = &mut h.app.screen {
        wizard.go(crate::onboarding::OnboardingStep::Finish);
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let text: String = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("google (google)"),
        "the finish summary: {text}"
    );
    assert!(text.contains("gemini-x"), "{text}");
    assert!(!text.contains("none yet"), "{text}");
}

/// The Models page asks for a catalog when it comes up without one,
/// however it was reached: the sidebar reaches it without going through
/// `/model`, and a page with nothing to list must not sit there quietly.
#[test]
fn entering_the_models_page_from_the_sidebar_fetches_an_empty_catalog() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    // Providers is the page /settings opens on; one tab reaches Models.
    let effects = h.app.on_key(key(KeyCode::Tab));
    assert_eq!(
        effects,
        vec![Effect::FetchModels(FetchWhy::Refresh)],
        "the empty page asks"
    );
    assert_eq!(
        models_page(&h).status,
        ModelPickerStatus::Loading,
        "and spins while it waits"
    );
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        remaining: 0,
        result: loaded("google", models()),
    });
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Ready);
    assert_eq!(picker.rows().len(), 2, "the answer lists");

    // The cache is warm now: coming back asks nothing.
    h.app.on_key(key(KeyCode::Tab));
    let effects = h.app.on_key(key(KeyCode::Tab));
    assert_eq!(effects, vec![], "a warm page asks nothing");
}

/// A batch reports one provider at a time: the rows that landed are up
/// while the rest are out, a dead provider is named as soon as it
/// answers, and the last report settles the page.
#[test]
fn a_catalog_lands_provider_by_provider() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/model");
    // The dead one answers first, with nothing.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        result: failed("llama-cpp", "connection refused"),
        remaining: 2,
    });
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Loading);
    assert_eq!(picker.failed, ["llama-cpp"], "the dead provider is named");
    // The first live catalog lands: its rows show while the rest are out.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        result: loaded("google", models()),
        remaining: 1,
    });
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Loading, "still out");
    assert_eq!(picker.rows().len(), 2, "the landed rows are up");
    assert!(picker.pickable(), "a landed row can be picked");
    // The last provider settles the page.
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Refresh,
        result: loaded("openrouter", vec![llama_model("z-ai/glm-5.3")]),
        remaining: 0,
    });
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Ready);
    assert_eq!(picker.rows().len(), 3);
    assert_eq!(picker.failed, ["llama-cpp"], "still named, still missing");
}

/// A batch already out answers the page that comes up: it spins on that
/// one instead of spending a second round trip on the same providers.
#[test]
fn a_page_that_comes_up_during_a_fetch_waits_on_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.catalog_inflight = true;
    let effects = crate::commands::dispatch(&mut h.app, "/model");
    assert_eq!(effects, vec![], "no second fetch");
    assert_eq!(
        models_page(&h).status,
        ModelPickerStatus::Loading,
        "but the page still spins"
    );
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        result: loaded("google", models()),
        remaining: 0,
    });
    assert!(!h.app.catalog_inflight, "the batch is done");
    let picker = models_page(&h);
    assert_eq!(picker.status, ModelPickerStatus::Ready);
    assert_eq!(picker.rows().len(), 2);
}

/// A click on the sidebar reaches Models the same way Tab does.
#[test]
fn a_click_on_the_models_row_fetches_an_empty_catalog() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let row_y = h
        .app
        .settings_hits
        .iter()
        .find_map(|(area, hit)| {
            matches!(
                hit,
                crate::app::SettingsHit::Sidebar(SettingsSection::Models)
            )
            .then_some(area.y)
        })
        .expect("the models row is clickable");
    let effects = h.app.on_click(4, row_y);
    assert_eq!(effects, vec![Effect::FetchModels(FetchWhy::Refresh)]);
    assert_eq!(models_page(&h).status, ModelPickerStatus::Loading);
}

#[test]
fn a_click_on_the_sidebar_switches_sections_and_a_row_acts() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    // Draw one frame so the renderer registers the hit rects, then
    // click the Appearance row (the sidebar's second section row).
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(&mut h.app, frame))
        .unwrap();
    let row_y = h
        .app
        .settings_hits
        .iter()
        .find_map(|(area, hit)| {
            matches!(
                hit,
                crate::app::SettingsHit::Sidebar(crate::screens::SettingsSection::Appearance)
            )
            .then_some(area.y)
        })
        .expect("the appearance row is clickable");
    h.app.on_click(4, row_y);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Appearance);
    // A click on a toggle row acts: the theme flips and persists.
    let (row_x, row_y) = h
        .app
        .settings_hits
        .iter()
        .find_map(|(area, hit)| {
            matches!(hit, crate::app::SettingsHit::PageRow(1)).then_some((area.x, area.y))
        })
        .expect("the theme row is clickable");
    h.app.on_click(row_x, row_y);
    assert_eq!(h.app.theme, crate::palette::LIGHT);
    assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
}

#[test]
fn settings_toggles_the_background_live_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert_eq!(h.app.theme, crate::palette::DARK);
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    // Row 2 is the background row; enter and `b` both flip it.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Char('b')));
    assert_eq!(
        h.app.options.background,
        crate::palette::Background::Native,
        "rhema flips to native"
    );
    assert_eq!(
        h.app.theme.sunken,
        ratatui::style::Color::Reset,
        "the fills hand back to the terminal"
    );
    assert_eq!(h.app.theme.text, crate::palette::DARK.text, "the ink keeps");
    assert_eq!(
        h.store.load().unwrap().background.as_deref(),
        Some("native")
    );
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(settings.background_native, "the page follows the toggle");
    }
    h.app.on_key(key(KeyCode::Enter));
    assert_eq!(h.app.options.background, crate::palette::Background::Rhema);
    assert_eq!(h.app.theme, crate::palette::DARK, "the fills come back");
    assert_eq!(h.store.load().unwrap().background.as_deref(), Some("rhema"));
}

#[test]
fn settings_cycles_the_cursor_style_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    // Row 4 is the cursor row; enter and `s` both cycle it, and the
    // effect carries the shape to the terminal this frame.
    for _ in 0..4 {
        h.app.on_key(key(KeyCode::Down));
    }
    assert_eq!(
        h.app.on_key(key(KeyCode::Enter)),
        vec![Effect::SetCursorStyle(crate::cursor::CursorStyle::Block)]
    );
    assert_eq!(h.app.options.cursor, crate::cursor::CursorStyle::Block);
    assert_eq!(h.store.load().unwrap().cursor.as_deref(), Some("block"));
    if let Screen::Settings(settings) = &h.app.screen {
        assert_eq!(
            settings.cursor,
            crate::cursor::CursorStyle::Block,
            "the page follows the toggle"
        );
    }
    // The cycle walks the explicit shapes, then wraps back to the
    // terminal's own caret.
    h.app.on_key(key(KeyCode::Char('s')));
    assert_eq!(h.app.options.cursor, crate::cursor::CursorStyle::Line);
    assert_eq!(h.store.load().unwrap().cursor.as_deref(), Some("line"));
    h.app.on_key(key(KeyCode::Char('s')));
    assert_eq!(h.app.options.cursor, crate::cursor::CursorStyle::Underline);
    assert_eq!(h.store.load().unwrap().cursor.as_deref(), Some("underline"));
    h.app.on_key(key(KeyCode::Char('s')));
    assert_eq!(h.app.options.cursor, crate::cursor::CursorStyle::Default);
    assert_eq!(h.store.load().unwrap().cursor.as_deref(), Some("default"));
}

#[test]
fn settings_toggles_the_scrollbar_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    // Row 5 is the scrollbar row; enter and `r` both flip it.
    for _ in 0..5 {
        h.app.on_key(key(KeyCode::Down));
    }
    h.app.on_key(key(KeyCode::Enter));
    assert!(!h.app.options.scrollbar, "the rail goes off");
    assert_eq!(h.store.load().unwrap().scrollbar, Some(false));
    if let Screen::Settings(settings) = &h.app.screen {
        assert!(!settings.scrollbar, "the page follows the toggle");
    }
    h.app.on_key(key(KeyCode::Char('r')));
    assert!(h.app.options.scrollbar, "the rail comes back");
    assert_eq!(h.store.load().unwrap().scrollbar, Some(true));
}

#[test]
fn settings_cycles_the_palette_live_and_persists_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert_eq!(h.app.theme, crate::palette::DARK);
    crate::commands::dispatch(&mut h.app, "/settings");
    tab_to(&mut h, SettingsSection::Appearance);
    h.app.on_key(key(KeyCode::Char('c')));
    assert_eq!(
        h.app.options.colors,
        crate::palette::Palette::Aquamarine,
        "rhema steps to aquamarine"
    );
    assert_eq!(h.app.theme, crate::palette::AQUAMARINE_DARK, "live swap");
    assert_eq!(
        h.store.load().unwrap().colors.as_deref(),
        Some("aquamarine")
    );
    // The appearance carries over: the palette's light tokens show.
    h.app.on_key(key(KeyCode::Char('t')));
    assert_eq!(h.app.theme, crate::palette::AQUAMARINE_LIGHT);
    // Cycling wraps back to rhema through every palette.
    for expected in [
        crate::palette::Palette::BlackWhite,
        crate::palette::Palette::Dracula,
        crate::palette::Palette::CatppuccinFrappe,
        crate::palette::Palette::CatppuccinMacchiato,
        crate::palette::Palette::CatppuccinMocha,
        crate::palette::Palette::Nord,
        crate::palette::Palette::Gruvbox,
        crate::palette::Palette::Solarized,
        crate::palette::Palette::M3You,
        crate::palette::Palette::Custom,
        crate::palette::Palette::Rhema,
    ] {
        h.app.on_key(key(KeyCode::Char('c')));
        assert_eq!(h.app.options.colors, expected);
    }
    assert_eq!(h.app.theme, crate::palette::LIGHT);
    assert_eq!(h.store.load().unwrap().colors.as_deref(), Some("rhema"));
}

#[test]
fn esc_while_entering_a_command_clears_it_without_quit_confirm() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    for c in "/clear".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Esc));
    assert!(h.app.chat.input.is_empty());
    assert!(!h.app.confirm_quit, "esc cancels the command, not the app");
}

#[test]
fn slash_model_during_a_turn_declines_with_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    crate::commands::dispatch(&mut h.app, "/model");
    assert!(matches!(h.app.screen, Screen::Chat));
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("waits for it to finish")
    ));
}

#[test]
fn slash_clear_forges_a_fresh_session() {
    let session = Session {
        history: vec![Message::text(Role::User, "earlier")],
        driver_state: serde_json::Value::Null,
        cost_usd: 0.0,
        usage_stats: serde_json::Value::Null,
    };
    let mut h = harness(Some("k"), Some("m-a"), session);
    crate::commands::dispatch(&mut h.app, "/clear");
    assert!(h.app.agent.unwrap().session().history.is_empty());
    assert!(
        h.app.transcript.lines().len() == 1,
        "only the notice remains"
    );
}

#[test]
fn slash_exit_quits_and_unknown_commands_get_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/nope");
    assert!(!h.app.should_quit);
    assert!(matches!(
        h.app.transcript.lines().last(),
        Some(Line::Notice(text)) if text.contains("unknown command /nope")
    ));
    crate::commands::dispatch(&mut h.app, "/exit");
    assert!(h.app.should_quit);
}

#[test]
fn the_status_tracker_follows_the_turn_lifecycle() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut tracker = crate::status::Tracker::default();

    // At rest before anything runs.
    assert!(matches!(
        tracker.status_for(&h.app),
        crate::status::Status::Idle
    ));

    // A turn runs: working, indeterminate, no progress fraction.
    h.app.turn.running = true;
    match tracker.status_for(&h.app) {
        crate::status::Status::Working { msg: None } => {}
        _ => panic!("a running turn is working"),
    }

    // The latest tool call names the working state.
    tracker.observe(&UiEvent::Engine(OwnedEvent::ToolCallStart {
        id: "c1".into(),
        name: "bash".into(),
    }));
    match tracker.status_for(&h.app) {
        crate::status::Status::Working { msg: Some(msg) } => assert_eq!(msg, "bash"),
        _ => panic!("the tool call is the working state's msg"),
    }

    // A pending approval blocks ahead of the running state.
    h.app.turn.pending_approval = Some(ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        args: serde_json::Value::Null,
        provider_data: serde_json::Value::Null,
    });
    assert!(matches!(
        tracker.status_for(&h.app),
        crate::status::Status::Blocked { .. }
    ));

    // The turn ends in failure: the error stands at the idle prompt,
    // first line only.
    let agent = h
        .app
        .agent
        .take()
        .expect("the keyed harness built an agent");
    h.app.turn.pending_approval = None;
    h.app.turn.running = false;
    tracker.observe(&UiEvent::TurnFinished {
        agent: Box::new(agent),
        result: Err("provider exploded\nwith detail".to_owned()),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    match tracker.status_for(&h.app) {
        crate::status::Status::Error { msg: Some(msg) } => {
            assert_eq!(msg, "provider exploded");
        }
        _ => panic!("a failed turn reads as an error at the idle prompt"),
    }

    // A later turn outranks the remembered failure and ends clean:
    // back to idle, not to the old error.
    h.app.turn.running = true;
    assert!(matches!(
        tracker.status_for(&h.app),
        crate::status::Status::Working { .. }
    ));
    let agent = HostAgent::new(HostAgentConfig::new(
        Arc::new(NeverDriven),
        "m-a",
        std::env::temp_dir(),
        rhema_host_agent::AllowAll,
    ))
    .unwrap();
    h.app.turn.running = false;
    tracker.observe(&UiEvent::TurnFinished {
        agent: Box::new(agent),
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(matches!(
        tracker.status_for(&h.app),
        crate::status::Status::Idle
    ));
}
/// A background subagent job pins its live line off the wire's
/// registry: working, it spins with its name, job id, and phase;
/// finished, the line leaves at once and the finish event names
/// the outcome; the fetch still serves the cached report. The
/// render reads the snapshot fresh, so the strip's reserve matches
/// what is on screen.
#[tokio::test]
async fn a_background_subagent_job_pins_its_live_line() {
    use rhema_core::{EventSink, ProviderError};
    use rhema_subagents::{SubagentDef, SubagentTools};

    /// Never answers: a job parked in its model stream.
    struct Hanging;
    /// Answers one report and ends.
    struct Report;

    #[async_trait::async_trait]
    impl ModelProvider for Hanging {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            std::future::pending().await
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for Report {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: Message::text(Role::Assistant, "the report"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn start_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "scout", "prompt": "look"}),
            ..ToolCall::default()
        }
    }

    fn fetch_call() -> ToolCall {
        ToolCall {
            id: "c2".into(),
            name: "subagent".into(),
            args: serde_json::json!({"id": "job-1"}),
            ..ToolCall::default()
        }
    }

    fn scout_tools(provider: Arc<dyn ModelProvider>) -> Arc<SubagentTools> {
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "scout".into(),
            description: "looks around".into(),
            model: None,
            max_steps: None,
            body: "Look.".into(),
        };
        Arc::new(SubagentTools::new(provider, vec![def], shell, "m"))
    }

    fn frame_text(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(app, frame))
            .unwrap();
        (0..40)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    let mut harness = harness(Some("key"), Some("m"), Session::default());

    // A running job: the line spins with its name, id, and phase,
    // even with no turn running, jobs outlive turns.
    let tools = scout_tools(Arc::new(Hanging));
    harness.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        start_call(),
        &rhema_core::ToolContext::default(),
    )
    .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness
        .app
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
    let text = frame_text(&mut harness.app);
    assert!(text.contains("scout (job-1)"), "{text}");
    assert!(text.contains("thinking"), "{text}");

    // Finished: the line leaves the strip at once; the finish
    // event names the outcome.
    let tools = scout_tools(Arc::new(Report));
    harness.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        start_call(),
        &rhema_core::ToolContext::default(),
    )
    .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let finished = loop {
        let events = harness.app.subagents.as_ref().unwrap().take_finished();
        if !events.is_empty() {
            break events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(finished[0].line, "Subagent scout (job-1) has finished.");
    let text = frame_text(&mut harness.app);
    assert!(
        !text.contains("scout (job-1)"),
        "a finished job is not pinned: {text}"
    );
    assert!(harness.app.subagent_jobs().is_empty());

    // The fetch still serves the cached report; the strip stays
    // empty.
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        fetch_call(),
        &rhema_core::ToolContext::default(),
    )
    .await;
    let text = frame_text(&mut harness.app);
    assert!(!text.contains("scout (job-1)"), "{text}");
}

/// A job finishing with no turn running runs its notification as
/// the next turn's prompt, the regular-prompt path. The tick
/// emits `StartTurn` with the notification line, the transcript
/// records it as the user line it is, and the take retires the
/// announcement: the driver's drain has nothing left to repeat,
/// and a second tick starts nothing.
#[tokio::test]
async fn an_idle_session_runs_a_finished_jobs_notification_as_the_next_prompt() {
    use rhema_core::{EventSink, ProviderError};
    use rhema_subagents::{SubagentDef, SubagentTools};

    /// Answers one report and ends.
    struct Report;

    #[async_trait::async_trait]
    impl ModelProvider for Report {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: Message::text(Role::Assistant, "the report"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    let mut h = harness(Some("key"), Some("m"), Session::default());
    let tools = {
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "scout".into(),
            description: "looks around".into(),
            model: None,
            max_steps: None,
            body: "Look.".into(),
        };
        Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
    };
    h.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "scout", "prompt": "look"}),
            ..ToolCall::default()
        },
        &rhema_core::ToolContext::default(),
    )
    .await;
    // A finished job left the snapshot; its finish event stays
    // pending for the tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while h
        .app
        .subagents
        .as_ref()
        .unwrap()
        .status()
        .iter()
        .any(|job| job.id == "job-1")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The idle tick: the notification becomes the prompt.
    let effects = h.app.on_tick();
    assert_eq!(effects.len(), 1);
    let Effect::StartTurn {
        text: prompt,
        images,
        ..
    } = &effects[0]
    else {
        panic!("the idle tick starts a turn, got {:?}", effects[0]);
    };
    assert_eq!(prompt, "Subagent scout (job-1) has finished.");
    assert!(images.is_empty());
    let last = h.app.transcript.lines().last();
    assert!(
        matches!(last, Some(Line::Notification(text)) if text == prompt.as_str()),
        "the notification card is the record, got {last:?}"
    );

    // A second tick starts nothing: the take retired the line.
    assert!(h.app.on_tick().is_empty());
    // The driver's drain is empty too, the prompt already said it.
    let steering = h.app.subagents.as_ref().unwrap().steering();
    assert!(steering.drain().is_empty(), "a taken line stops announcing");
}

/// A subagent job finishing while a turn runs lands as its card and
/// leaves the delivery to the driver's steering: the tick starts
/// nothing and takes no notification line, so the drain still names
/// the job, the registry's drain stays a query, the fetch or the
/// idle take retiring it.
#[tokio::test]
async fn a_running_turn_shows_the_finish_card_and_leaves_steering_to_the_driver() {
    use rhema_core::{EventSink, ProviderError};
    use rhema_subagents::{SubagentDef, SubagentTools};

    /// Answers one report and ends.
    struct Report;

    #[async_trait::async_trait]
    impl ModelProvider for Report {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: Message::text(Role::Assistant, "the report"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    let mut h = harness(Some("key"), Some("m"), Session::default());
    let tools = {
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "reviewer".into(),
            description: "reviews".into(),
            model: None,
            max_steps: None,
            body: "Review.".into(),
        };
        Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
    };
    h.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "reviewer", "prompt": "review"}),
            ..ToolCall::default()
        },
        &rhema_core::ToolContext::default(),
    )
    .await;
    h.app.turn.running = true;
    // A finished job left the snapshot; its finish event stays
    // pending for the tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while h
        .app
        .subagents
        .as_ref()
        .unwrap()
        .status()
        .iter()
        .any(|job| job.id == "job-1")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let effects = h.app.on_tick();
    assert!(
        effects.is_empty(),
        "a running turn takes no prompt: {effects:?}"
    );
    let last = h.app.transcript.lines().last();
    assert!(
        matches!(last, Some(Line::Notification(text))
                if text == "Subagent reviewer (job-1) has finished."),
        "the finish lands as its card, got {last:?}"
    );
    // The driver still steers: the announcement line is pending.
    let steering = h.app.subagents.as_ref().unwrap().steering();
    assert_eq!(
        steering.drain(),
        vec!["Subagent reviewer (job-1) has finished.".to_owned()]
    );
}

/// A background job finishing while the session is idle becomes the
/// next prompt: the tick delivers the line as a turn, records it as
/// its notification card, never as user input, and the take retires
/// it so neither the driver's drain nor a second tick repeats it.
#[test]
fn an_idle_session_runs_a_finished_job_line_as_the_next_prompt() {
    let mut h = harness(Some("key"), Some("m"), Session::default());
    let line = "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned();
    h.app.jobs = Some(crate::jobs::JobsHandle::stub_with_finished(vec![
        crate::jobs::JobFinished {
            id: "abc".to_owned(),
            command: "npm run dev".to_owned(),
            exit_code: Some(0),
            line: line.clone(),
        },
    ]));
    let effects = h.app.on_tick();
    assert_eq!(effects.len(), 1);
    let Effect::StartTurn {
        text: prompt,
        images,
        ..
    } = &effects[0]
    else {
        panic!("the idle tick starts a turn, got {:?}", effects[0]);
    };
    assert_eq!(prompt, &line);
    assert!(images.is_empty());
    let last = h.app.transcript.lines().last();
    assert!(
        matches!(last, Some(Line::Notification(text)) if text == line.as_str()),
        "the notification card is the record, got {last:?}"
    );
    // A second tick starts nothing: the take retired the line.
    assert!(h.app.on_tick().is_empty());
    let steering = h.app.jobs.as_ref().unwrap().steering();
    assert!(steering.drain().is_empty(), "a taken line stops announcing");
}

/// A background job finishing while a turn runs lands as its card and
/// leaves the delivery to the driver's steering, which consumes the
/// line, so the idle prompt never repeats it: the tick starts nothing,
/// the drain names the job once.
#[test]
fn a_running_turn_shows_a_job_finish_card_and_leaves_one_delivery_to_the_driver() {
    let mut h = harness(Some("key"), Some("m"), Session::default());
    let line =
        "Background job abc (\"npm run dev\") has finished with an error (exit 1).".to_owned();
    h.app.jobs = Some(crate::jobs::JobsHandle::stub_with_finished(vec![
        crate::jobs::JobFinished {
            id: "abc".to_owned(),
            command: "npm run dev".to_owned(),
            exit_code: Some(1),
            line: line.clone(),
        },
    ]));
    h.app.turn.running = true;
    let effects = h.app.on_tick();
    assert!(
        effects.is_empty(),
        "a running turn takes no prompt: {effects:?}"
    );
    let last = h.app.transcript.lines().last();
    assert!(
        matches!(last, Some(Line::Notification(text)) if text == line.as_str()),
        "the finish lands as its card, got {last:?}"
    );
    // The driver steers the delivery, once, the drain consuming it.
    let steering = h.app.jobs.as_ref().unwrap().steering();
    assert_eq!(steering.drain(), vec![line]);
    assert!(h.app.jobs.as_ref().unwrap().take_notifications().is_empty());
}

/// A finish the driver never steered — minted after the turn's last
/// step boundary — comes back after the turn ends as the idle prompt
/// only: the detection already recorded its card, so the delivery
/// must not push it a second time, and a further tick starts nothing.
#[test]
fn a_finish_missed_by_the_steering_delivers_its_prompt_without_a_second_card() {
    let mut h = harness(Some("key"), Some("m"), Session::default());
    let line = "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned();
    let cards = |h: &Harness| {
        h.app
            .transcript
            .lines()
            .iter()
            .filter(|entry| matches!(entry, Line::Notification(text) if *text == line))
            .count()
    };
    h.app.jobs = Some(crate::jobs::JobsHandle::stub_with_finished(vec![
        crate::jobs::JobFinished {
            id: "abc".to_owned(),
            command: "npm run dev".to_owned(),
            exit_code: Some(0),
            line: line.clone(),
        },
    ]));
    // The finish lands while a turn runs: the card is the record.
    h.app.turn.running = true;
    assert!(h.app.on_tick().is_empty());
    assert_eq!(cards(&h), 1, "the detection card");
    // The turn ends without the driver draining: the idle tick
    // delivers the line, and no second card.
    h.app.turn.running = false;
    let effects = h.app.on_tick();
    let assert_matches = |effects: &[Effect]| {
        assert_eq!(effects.len(), 1, "the idle tick starts one turn");
        let Effect::StartTurn { text, images, .. } = &effects[0] else {
            panic!("the idle tick starts a turn, got {:?}", effects[0]);
        };
        assert_eq!(text, &line);
        assert!(images.is_empty());
    };
    assert_matches(&effects);
    assert_eq!(cards(&h), 1, "the delivery repeated the detection's card");
    // A further tick starts nothing: the take retired the line.
    assert!(h.app.on_tick().is_empty());
    assert_eq!(cards(&h), 1);
}

/// Both feeds pending and the session idle: one prompt carries both
/// announcements, subagents first, background jobs second, the
/// steering chain's order, and neither feed swallowed by the other.
#[tokio::test]
async fn both_feeds_join_into_one_idle_prompt_with_subagents_first() {
    use rhema_core::ProviderError;
    use rhema_subagents::{SubagentDef, SubagentTools};

    /// Answers one report and ends.
    struct Report;

    #[async_trait::async_trait]
    impl ModelProvider for Report {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn rhema_core::EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: Message::text(Role::Assistant, "the report"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    let mut h = harness(Some("key"), Some("m"), Session::default());
    let tools = {
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "reviewer".into(),
            description: "reviews".into(),
            model: None,
            max_steps: None,
            body: "Review.".into(),
        };
        Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
    };
    h.app.subagents = Some(tools.handle());
    let job_line = "Background job abc (\"npm run dev\") has finished (exit 0).".to_owned();
    h.app.jobs = Some(crate::jobs::JobsHandle::stub_with_finished(vec![
        crate::jobs::JobFinished {
            id: "abc".to_owned(),
            command: "npm run dev".to_owned(),
            exit_code: Some(0),
            line: job_line.clone(),
        },
    ]));
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "reviewer", "prompt": "review"}),
            ..ToolCall::default()
        },
        &rhema_core::ToolContext::default(),
    )
    .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while h
        .app
        .subagents
        .as_ref()
        .unwrap()
        .status()
        .iter()
        .any(|job| job.id == "job-1")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let effects = h.app.on_tick();
    assert_eq!(effects.len(), 1, "the idle tick starts one turn");
    let Effect::StartTurn { text: prompt, .. } = &effects[0] else {
        panic!("the idle tick starts a turn, got {:?}", effects[0]);
    };
    let expected = "Subagent reviewer (job-1) has finished.\n".to_owned() + &job_line;
    assert_eq!(prompt.as_str(), expected);
}

/// A turn that failed with a network error leaves the offer up:
/// the error strip rides the hint, and `/retry` re-issues the same
/// request, no new prompt, the flag spent.
#[test]
fn a_failed_turn_offers_retry_and_retry_fires_it() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Err("network error: connection reset".to_owned()),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(h.app.retryable);
    assert_eq!(
        h.app.error.as_deref(),
        Some("network error: connection reset")
    );

    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert_eq!(effects, vec![Effect::RetryTurn]);
    assert!(h.app.turn.running, "the retry takes the turn slot");
    assert!(!h.app.retryable, "the offer is spent");
    assert!(h.app.error.is_none());
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("retrying".into())),
        "the notice marks the fresh output block"
    );
}

/// `/retry` restarts the subagent jobs the network killed, offer
/// or no offer: a job can die to an outage the parent turn
/// survived, and the notice names what came back. Nothing failed
/// elsewhere, so no turn starts; a second dispatch has nothing
/// left to restart and says so.
#[tokio::test]
async fn retry_restarts_transient_failed_subagents() {
    use rhema_core::{EventSink, ProviderError};
    use rhema_subagents::{SubagentDef, SubagentTools};

    /// Fails the first call with a dead link, then answers.
    struct Flaky(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl ModelProvider for Flaky {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            if !self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                // Already spent: the link recovered.
                return Ok(rhema_core::ModelOutput {
                    message: Message::text(Role::Assistant, "the report"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                });
            }
            Err(ProviderError::Network(
                "error decoding response body".into(),
            ))
        }
    }

    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
    let def = SubagentDef {
        name: "scout".into(),
        description: "looks around".into(),
        model: None,
        max_steps: None,
        body: "Look.".into(),
    };
    let tools = Arc::new(
        SubagentTools::new(Arc::new(Flaky(true.into())), vec![def], shell, "m")
            .with_retry_ladders(vec![], vec![]),
    );
    h.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "scout", "prompt": "look"}),
            ..ToolCall::default()
        },
        &rhema_core::ToolContext::default(),
    )
    .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if h.app.subagents.as_ref().unwrap().status().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The failure notice drains on the tick a real frontend runs;
    // the test takes it by hand.
    let _ = h.app.subagents.as_ref().unwrap().take_finished();

    // The restart: no failed turn, so no turn, but the job comes
    // back under its own id and the notice names it.
    assert!(!h.app.retryable);
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        h.app.transcript.lines().contains(&Line::Notice(
            "restarted 1 subagent after network failures: scout (job-1)".into()
        )),
        "{:?}",
        h.app.transcript.lines().last()
    );

    // The re-run answers: the finish announces into the same id.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let events = loop {
        let events = h.app.subagents.as_ref().unwrap().take_finished();
        if !events.is_empty() {
            break events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the re-run never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(events[0].line, "Subagent scout (job-1) has finished.");

    // Spent: a second dispatch is the plain no-offer answer.
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert!(effects.is_empty());
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("no failed turn to retry".into()))
    );

    // With a failed turn behind it as well, one dispatch restarts
    // the jobs and fires the turn, the whole outage rewound.
    h.app.retryable = true;
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert_eq!(effects, vec![Effect::RetryTurn]);
}

/// Everything that moves the conversation on clears the offer: a
/// fresh submit, a finished turn, a cancel, and a cleared
/// conversation each leave `/retry` with nothing to re-issue.
#[test]
fn the_retry_offer_clears_when_the_conversation_moves_on() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let steer = tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1;

    // Nothing failed yet.
    assert!(!h.app.retryable);
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert!(effects.is_empty());
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("no failed turn to retry".into()))
    );

    // The failed turn raises it; the next submit spends it.
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Err("network error: connection reset".to_owned()),
        steer,
    });
    h.app.on_key(key(KeyCode::Char('a')));
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert!(matches!(effects.as_slice(), [Effect::StartTurn { .. }]));
    assert!(!h.app.retryable);
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert!(effects.is_empty());

    // A finished turn's outcome rules the flag either way: success
    // clears, failure raises again.
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.retryable);

    // A cancel is a choice, not a failure.
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnCancelled {
        agent,
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    assert!(!h.app.retryable);
    assert!(!matches!(
        crate::commands::dispatch(&mut h.app, "/retry").as_slice(),
        [Effect::RetryTurn]
    ));

    // Clearing the conversation retires it too.
    h.app.retryable = true;
    h.app.clear_session();
    assert!(!h.app.retryable);
}

/// The offer survives an agent rebuild on purpose: a failed turn,
/// then a model switch, then `/retry` re-issues the request on the
/// new model, the rate-limit answer.
#[test]
fn a_model_switch_keeps_the_retry_offer() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.on_event(UiEvent::ModelsLoaded {
        why: FetchWhy::Bootstrap,
        remaining: 0,
        result: loaded("google", models()),
    });
    let agent = Box::new(h.app.agent.take().unwrap());
    h.app.on_event(UiEvent::TurnFinished {
        agent,
        result: Err("provider api error: 429 (retry-after: 37)".to_owned()),
        steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
    });
    crate::commands::dispatch(&mut h.app, "/model google/m-b");
    assert!(h.app.agent.is_some(), "the pick rebuilt the agent");
    assert!(h.app.retryable, "the switch is not a conversation change");
    assert_eq!(
        crate::commands::dispatch(&mut h.app, "/retry"),
        vec![Effect::RetryTurn]
    );
}

/// While a turn runs, a retry included, `/retry` waits.
#[test]
fn retry_waits_while_a_turn_runs() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.turn.running = true;
    let effects = crate::commands::dispatch(&mut h.app, "/retry");
    assert!(effects.is_empty());
    assert!(h.app.transcript.lines().contains(&Line::Notice(
        "a turn is running; /retry waits for it to finish".into()
    )));
}

// --- MCP page ---

/// A stdio server stored on the harness's profile.
fn store_mcp_server(h: &mut Harness, name: &str) {
    h.app.config.set_for_profile(Some("personal"), |profile| {
        profile.mcp_servers.insert(
            name.to_owned(),
            rhema_config::mcp::McpServerEntry {
                description: "Jira and Confluence".to_owned(),
                command: Some(vec!["npx".to_owned(), "-y".to_owned(), "server".to_owned()]),
                url: None,
                env: BTreeMap::new(),
                headers: BTreeMap::new(),
                startup_timeout: None,
                timeout: None,
                command_name: None,
            },
        );
    });
    h.store.save(&h.app.config).unwrap();
}

/// The profile the store holds, reloaded.
fn stored_profile(h: &Harness) -> rhema_config::Profile {
    h.store.load().unwrap().effective(Some("personal"))
}

#[test]
fn slash_mcp_opens_the_settings_mcp_page() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    store_mcp_server(&mut h, "atlassian");
    crate::commands::dispatch(&mut h.app, "/mcp");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Mcp);
    assert_eq!(
        settings
            .mcp
            .iter()
            .map(|row| (row.name.as_str(), row.transport))
            .collect::<Vec<_>>(),
        vec![("atlassian", Some("stdio"))],
        "the page opens over the profile's servers"
    );
    // An empty profile opens to the add row and nothing else.
    let mut fresh = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut fresh.app, "/mcp");
    let Screen::Settings(settings) = &fresh.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.mcp.is_empty(), "the empty profile lists nothing");
    assert_eq!(settings.row, 0, "the add row is the only row");
}

#[test]
fn the_mcp_add_dialog_inserts_and_persists() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    crate::commands::dispatch(&mut h.app, "/mcp");
    // The add row is the only row: enter opens the dialog.
    h.app.on_key(key(KeyCode::Enter));
    // Name.
    for c in "atlassian".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    // Description.
    for c in "Jira and Confluence".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    // stdio is the highlighted transport: enter takes it, then the
    // argv line.
    h.app.on_key(key(KeyCode::Enter));
    for c in "npx -y @mcp/server".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    // One env pair, then the blank line's enter moves on.
    for c in "JIRA_TOKEN=t".chars() {
        h.app.on_key(key(KeyCode::Char(c)));
    }
    h.app.on_key(key(KeyCode::Enter));
    h.app.on_key(key(KeyCode::Enter));
    // The bash-name stage: enter with nothing typed stores the entry
    // unnamed.
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert!(
        effects.is_empty(),
        "the store needs no effects: {effects:?}"
    );
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.mcp_dialog.is_none(), "the dialog spent itself");
    let entry = &settings.stored.mcp_servers["atlassian"];
    assert_eq!(entry.description, "Jira and Confluence");
    assert_eq!(entry.env.get("JIRA_TOKEN").map(String::as_str), Some("t"));
    // The store holds it: the profile write went through save_config.
    let stored = stored_profile(&h);
    assert_eq!(stored.mcp_servers.len(), 1);
    assert_eq!(
        stored.mcp_servers["atlassian"].command,
        Some(vec![
            "npx".to_owned(),
            "-y".to_owned(),
            "@mcp/server".to_owned()
        ])
    );
    // The page's rows follow the config: one server and the add row.
    assert_eq!(
        settings
            .mcp
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        vec!["atlassian"]
    );
    assert!(
        settings.row <= settings.mcp.len(),
        "the selection stays inside"
    );
}

#[test]
fn the_mcp_remove_confirms_then_deletes_and_persists() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    store_mcp_server(&mut h, "atlassian");
    crate::commands::dispatch(&mut h.app, "/mcp");
    // d arms the confirm; the profile still holds the server.
    h.app.on_key(key(KeyCode::Char('d')));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.mcp_confirm.as_deref(), Some("atlassian"));
    assert_eq!(
        stored_profile(&h).mcp_servers.len(),
        1,
        "d alone removes nothing"
    );
    // Anything but y and n keeps asking.
    h.app.on_key(key(KeyCode::Char('x')));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.mcp_confirm.as_deref(), Some("atlassian"));
    // n cancels, then d and y remove for real.
    h.app.on_key(key(KeyCode::Char('n')));
    h.app.on_key(key(KeyCode::Char('d')));
    h.app.on_key(key(KeyCode::Char('y')));
    let stored = stored_profile(&h);
    assert!(
        stored.mcp_servers.is_empty(),
        "the confirm removed it: {:?}",
        stored.mcp_servers.keys().collect::<Vec<_>>()
    );
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.mcp_confirm, None, "the confirm spent itself");
    assert!(settings.mcp.is_empty(), "the rows follow the config");
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("removed the atlassian server".into())),
        "the removal is noticed"
    );
}

#[test]
fn the_mcp_mute_round_trips() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    store_mcp_server(&mut h, "atlassian");
    crate::commands::dispatch(&mut h.app, "/mcp");
    // Enter on the row mutes: the set holds the name and the row
    // marks it, while the profile keeps the entry.
    h.app.on_key(key(KeyCode::Enter));
    assert!(
        h.app.disabled_mcp_servers.contains("atlassian"),
        "the mute lands in the set"
    );
    assert_eq!(stored_profile(&h).mcp_servers.len(), 1, "the entry stays");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.mcp[0].disabled, "the row marks the mute");
    assert!(
        h.app.transcript.lines().contains(&Line::Notice(
            "the atlassian server is muted, saved and applied to new turns".into()
        )),
        "the mute is noticed"
    );
    // The mute persists: the config field holds it, the same set the
    // agent rebuild reads.
    assert!(
        h.app.config.disabled_mcp_servers.contains("atlassian"),
        "the mute lands in the config"
    );
    // Enter again unmutes, back where it started.
    h.app.on_key(key(KeyCode::Enter));
    assert!(h.app.disabled_mcp_servers.is_empty(), "the mute lifts");
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(!settings.mcp[0].disabled, "the row clears its mark");
    // A removed server cannot stay muted behind it.
    h.app.disabled_mcp_servers.insert("atlassian".to_owned());
    h.app.on_key(key(KeyCode::Char('d')));
    h.app.on_key(key(KeyCode::Char('y')));
    assert!(
        !h.app.disabled_mcp_servers.contains("atlassian"),
        "the mute dies with the server"
    );
}

#[test]
fn the_update_row_installs_a_known_release() {
    let mut h = harness(None, None, Session::default());
    // No answer yet: the page is not in the sidebar, and the open lands
    // on the first section instead. The one act of the page installs a
    // newer release; with none known, the page has nothing to say.
    h.app.open_settings_at(SettingsSection::Update);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(!settings.sections().contains(&SettingsSection::Update));
    assert_eq!(settings.section, SettingsSection::Providers);

    // The check answers with a release above this build: the state and
    // the open page follow, and the install row carries its action.
    h.app.on_event(UiEvent::UpdateChecked {
        result: Ok("99.0.0".to_owned()),
    });
    h.app.open_settings_at(SettingsSection::Update);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(
        settings.update.check,
        crate::screens::UpdateCheck::Done(Ok("99.0.0".to_owned()))
    );
    assert!(
        settings.update.update_available(),
        "99.0.0 is above any build in tests"
    );
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::InstallUpdate]);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(
        settings.update.install,
        crate::screens::UpdateInstall::Running
    );

    // The install run answers: the page shows it done and the
    // transcript carries the notice a closed page would show.
    h.app.on_event(UiEvent::UpdateInstalled {
        result: Ok("installed rhema 99.0.0 to ~/.local/bin/rhema".to_owned()),
    });
    assert!(matches!(
        h.app.update.install,
        crate::screens::UpdateInstall::Done(Ok(_))
    ));
    assert!(
        h.app.transcript.lines().contains(&Line::Notice(
            "rhema updated, restart to run the new version".into()
        )),
        "the install is noticed"
    );
    assert!(
        !h.app.update.update_pending(),
        "a landed install retires the banner's ask"
    );

    // A second Enter mid-run does not stack installers: a fresh
    // harness with the state mid-run re-checks the guard alone.
    h.app.update.install = crate::screens::UpdateInstall::Running;
    h.app.sync_update_page();
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert!(effects.is_empty(), "the running install refuses re-enters");
    // A finished run may run again: the installer is idempotent, and
    // a retry after a failure is the point.
    h.app.update.install = crate::screens::UpdateInstall::Done(Ok("done".to_owned()));
    h.app.sync_update_page();
    let effects = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(effects, vec![Effect::InstallUpdate]);
}

#[test]
fn the_update_page_is_listed_only_while_a_release_exists() {
    let mut h = harness(None, None, Session::default());
    h.app.open_settings_at(SettingsSection::Providers);

    // No answer yet: the sidebar holds every page but Update.
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(!settings.sections().contains(&SettingsSection::Update));
    assert_eq!(
        settings.sections().len(),
        SettingsSection::ALL.len() - 1,
        "only the Update page is held back"
    );

    // A check that failed lists it no more than none at all.
    h.app.on_event(UiEvent::UpdateChecked {
        result: Err("no answer within 30 s".to_owned()),
    });
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(!settings.sections().contains(&SettingsSection::Update));
    // And asking for it lands on the first page rather than a page the
    // sidebar does not list.
    h.app.open_settings_at(SettingsSection::Update);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(settings.section, SettingsSection::Providers);

    // A channel at or below this build is no update either.
    h.app.on_event(UiEvent::UpdateChecked {
        result: Ok("0.0.1".to_owned()),
    });
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(
        !settings.update.update_available(),
        "the channel trails this build"
    );
    assert!(!settings.sections().contains(&SettingsSection::Update));

    // A release above this build lists it, and stepping off it after a
    // landed install retires it again.
    h.app.on_event(UiEvent::UpdateChecked {
        result: Ok("99.0.0".to_owned()),
    });
    h.app.open_settings_at(SettingsSection::Update);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert!(settings.sections().contains(&SettingsSection::Update));
    h.app.on_event(UiEvent::UpdateInstalled {
        result: Ok("installed rhema 99.0.0 to ~/.local/bin/rhema".to_owned()),
    });
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    // The open page stays readable with the install's verdict on it.
    assert!(settings.sections().contains(&SettingsSection::Update));
    assert_eq!(settings.section, SettingsSection::Update);
    // Stepping away retires it: nothing is left to install.
    h.app.on_key(key(KeyCode::Tab));
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_ne!(settings.section, SettingsSection::Update);
    assert!(!settings.sections().contains(&SettingsSection::Update));
}

/// `/update` is the whole flow in one word: the check when this session
/// has no answer, and the install of the release it finds.
#[test]
fn the_update_command_checks_then_installs() {
    let mut h = harness(None, None, Session::default());
    let effects = crate::commands::dispatch(&mut h.app, "/update");
    assert_eq!(effects, vec![Effect::CheckForUpdates]);
    assert_eq!(
        h.app.update.check,
        crate::screens::UpdateCheck::Checking,
        "the command marks the check running"
    );
    assert!(
        h.app.update.install_after_check,
        "the install waits on the answer"
    );

    // A newer release lands: the install follows without a second word.
    let effects = h.app.on_event(UiEvent::UpdateChecked {
        result: Ok("99.0.0".to_owned()),
    });
    assert_eq!(effects, vec![Effect::InstallUpdate]);
    assert_eq!(h.app.update.install, crate::screens::UpdateInstall::Running);
    assert!(!h.app.update.install_after_check, "the promise is spent");

    // A known release skips the check: the same word installs at once.
    h.app.update.check = crate::screens::UpdateCheck::Done(Ok("99.0.0".to_owned()));
    h.app.update.install = crate::screens::UpdateInstall::Idle;
    let effects = crate::commands::dispatch(&mut h.app, "/update");
    assert_eq!(effects, vec![Effect::InstallUpdate]);

    // A running install takes nothing: one installer at a time.
    let effects = crate::commands::dispatch(&mut h.app, "/update");
    assert!(effects.is_empty(), "the running install refuses a second");
    assert!(
        h.app
            .transcript
            .lines()
            .contains(&Line::Notice("an update is already running".into())),
        "the second ask is answered"
    );
}

/// A channel at this build answers `/update` with the fact and no
/// install, and a failed check says why.
#[test]
fn the_update_command_stops_when_nothing_is_newer() {
    let mut h = harness(None, None, Session::default());
    crate::commands::dispatch(&mut h.app, "/update");
    let effects = h.app.on_event(UiEvent::UpdateChecked {
        result: Ok(crate::update::current_version().to_owned()),
    });
    assert!(effects.is_empty(), "nothing to install");
    assert_eq!(
        h.app.update.install,
        crate::screens::UpdateInstall::Idle,
        "no install starts"
    );
    assert!(
        h.app.transcript.lines().iter().any(|line| matches!(
            line,
            Line::Notice(text) if text.contains("is the newest release")
        )),
        "the answer is the fact: {:?}",
        h.app.transcript.lines()
    );

    let mut h = harness(None, None, Session::default());
    crate::commands::dispatch(&mut h.app, "/update");
    h.app.on_event(UiEvent::UpdateChecked {
        result: Err("no answer within 30 s".to_owned()),
    });
    assert!(h.app.transcript.lines().iter().any(|line| matches!(
        line,
        Line::Notice(text) if text.contains("the update check failed: no answer")
    )));
}

#[test]
fn the_update_states_survive_closing_the_settings() {
    let mut h = harness(None, None, Session::default());
    // The startup check marks itself running before the first frame.
    h.app.initial_effects();
    h.app.open_settings_at(SettingsSection::Update);
    // Back to the chat and in again: the check's in-flight state
    // rides with the app, not the page snapshot.
    h.app.on_key(key(KeyCode::Esc));
    assert!(matches!(h.app.screen, Screen::Chat));
    h.app.open_settings_at(SettingsSection::Update);
    let Screen::Settings(settings) = &h.app.screen else {
        panic!("expected the settings screen");
    };
    assert_eq!(
        settings.update.check,
        crate::screens::UpdateCheck::Checking,
        "the opener syncs the app's state in"
    );
}

#[test]
fn a_clear_with_the_agent_out_leaves_nothing_to_resume() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    // A live conversation in the agent.
    if let Some(agent) = h.app.agent.as_mut() {
        agent
            .session_mut()
            .history
            .push(Message::text(Role::User, "the whole conversation"));
        agent
            .session_mut()
            .history
            .push(Message::text(Role::Assistant, "its answer"));
    }
    // A keyless profile moves in: the agent drops, and the session comes
    // home to the App's own copy (the switch keeps the conversation).
    h.app.config.profiles.insert(
        "work".to_owned(),
        rhema_config::Profile {
            provider: Some("openrouter".into()),
            ..rhema_config::Profile::default()
        },
    );
    let _ = h.app.switch_profile("work");
    assert!(h.app.agent.is_none(), "the agent drops with the provider");
    assert_eq!(h.app.session.history.len(), 2, "the switch keeps the talk");

    // The clear lands while the agent is out. The exit save reads the
    // App's copy, a stale copy here would export the cleared
    // conversation, and with the file binding gone it would grow a
    // second session file for it.
    let _ = crate::commands::dispatch(&mut h.app, "/clear");
    assert_eq!(
        h.app.export_session(),
        None,
        "nothing of the cleared talk exports"
    );
}

#[test]
fn a_clear_survives_a_switch_back() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    if let Some(agent) = h.app.agent.as_mut() {
        agent
            .session_mut()
            .history
            .push(Message::text(Role::User, "the whole conversation"));
    }
    // Away to a keyless profile (the agent drops), a clear, and back to
    // the keyed one: the rebuild reads the App's copy, so a stale copy
    // would continue the cleared conversation from its next turn.
    h.app.config.profiles.insert(
        "work".to_owned(),
        rhema_config::Profile {
            provider: Some("openrouter".into()),
            ..rhema_config::Profile::default()
        },
    );
    let _ = h.app.switch_profile("work");
    let _ = crate::commands::dispatch(&mut h.app, "/clear");
    let _ = h.app.switch_profile("personal");
    let len = h
        .app
        .agent
        .as_ref()
        .map(|agent| agent.session().history.len())
        .expect("the keyed profile rebuilds the agent");
    assert_eq!(len, 0, "the cleared conversation stays cleared");
}

#[test]
fn adding_a_websearch_provider_places_its_key_and_enrolls_it() {
    let mut h = harness_with_secrets(None, None, Session::default());
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    h.app
        .open_settings_at(crate::screens::SettingsSection::WebSearch);
    // The add row is selected on the empty page; enter opens the
    // dialog.
    let action = h.app.on_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        vec![],
        "the keypress routed through on_key, no effects yet"
    );
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        assert!(
            settings.websearch_dialog.is_some(),
            "enter on the add row opened the dialog"
        );
        // Kind: brave (the first row), enter accepts, the name line
        // carries the suggestion, edited to brave-main.
        let dialog = settings.websearch_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Enter));
        for _ in 0.."brave".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "brave-main".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Enter));
        for c in "sk-1".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }
    // Enter routes the dialog's Submit through the app: the config
    // write, the store placement, the persist.
    let effects = h.app.on_key(key(KeyCode::Enter));
    let _ = effects;
    let stored = h.store.load().unwrap().effective(Some("personal"));
    let instance = &stored.websearch["brave-main"];
    assert_eq!(instance.provider, "brave");
    assert_eq!(instance.api_key, None, "the file holds no key");
    let id = instance.secret_ref.as_deref().expect("the store ref");
    assert_eq!(secrets.get(id).unwrap().as_deref(), Some("sk-1"));
    assert_eq!(
        stored.websearch_order,
        vec!["brave-main".to_owned()],
        "a first provider enrolls at the chain's end"
    );
}

#[test]
fn a_second_provider_of_the_same_kind_joins_the_chain() {
    // Three kinds on the dialog's list is a catalog, not a quota:
    // the same vendor can be added again and again, each with its
    // own key, the name stage numbering the suggestions.
    let mut h = harness_with_secrets(None, None, Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::WebSearch);
    // First pass: Exa, keep the suggested name, type a key.
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.websearch_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Down)); // exa
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter)); // keep "Exa"
        for c in "key-one".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }
    h.app.on_key(key(KeyCode::Enter));
    // Second pass: down to the add row (one provider row above it),
    // Exa again, the suggestion reads "Exa 2".
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.websearch_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(
            dialog.input, "Exa 2",
            "the suggestion numbers past the taken"
        );
        dialog.handle_key(key(KeyCode::Enter));
        for c in "key-two".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }
    h.app.on_key(key(KeyCode::Enter));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.websearch.len(),
        2,
        "two Exa instances, distinct names, one chain"
    );
    assert_eq!(stored.websearch["Exa"].provider, "exa");
    assert_eq!(stored.websearch["Exa 2"].provider, "exa");
    assert_eq!(
        stored.websearch_order,
        vec!["Exa".to_owned(), "Exa 2".to_owned()],
        "each add enrolls at the chain's end, in add order"
    );
}

#[test]
fn the_websearch_order_moves_and_persists_without_a_rebuild() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |p| {
        p.websearch.insert(
            "brave-main".into(),
            rhema_config::WebSearchInstance {
                provider: "brave".into(),
                api_key: Some("k".into()),
                secret_ref: None,
            },
        );
        p.websearch.insert(
            "exa-work".into(),
            rhema_config::WebSearchInstance {
                provider: "exa".into(),
                api_key: Some("k".into()),
                secret_ref: None,
            },
        );
        p.websearch_order = vec!["brave-main".into(), "exa-work".into()];
    });
    let _ = h.store.save(&h.app.config.clone());
    h.app
        .open_settings_at(crate::screens::SettingsSection::WebSearch);
    // Row 0 is brave-main; j sends it down one.
    h.app.on_key(key(KeyCode::Char('j')));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.websearch_order,
        vec!["exa-work".to_owned(), "brave-main".to_owned()]
    );
    // The selection rides the rebuilt rows: row 0 is now exa-work,
    // row 1 brave-main. Down to it, then k walks it back up.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Char('k')));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.websearch_order,
        vec!["brave-main".to_owned(), "exa-work".to_owned()]
    );
    // The selection stays at row 1, which now holds exa-work: a
    // second k walks THAT up, the swap toggles under a still
    // selection.
    h.app.on_key(key(KeyCode::Char('k')));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.websearch_order,
        vec!["exa-work".to_owned(), "brave-main".to_owned()]
    );
    // k at the top stays put: the chain never loses a member this
    // way.
    h.app.on_key(key(KeyCode::Up));
    h.app.on_key(key(KeyCode::Char('k')));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.websearch_order,
        vec!["exa-work".to_owned(), "brave-main".to_owned()]
    );
}

#[test]
fn removing_a_websearch_provider_takes_its_order_entry_with_it() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |p| {
        p.websearch.insert(
            "brave-main".into(),
            rhema_config::WebSearchInstance {
                provider: "brave".into(),
                api_key: Some("k".into()),
                secret_ref: None,
            },
        );
        p.websearch_order = vec!["brave-main".into()];
    });
    let _ = h.store.save(&h.app.config.clone());
    h.app
        .open_settings_at(crate::screens::SettingsSection::WebSearch);
    h.app.on_key(key(KeyCode::Char('d')));
    assert_eq!(
        h.app.on_key(key(KeyCode::Char('y'))),
        vec![],
        "the confirm routed through on_key"
    );
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert!(stored.websearch.is_empty());
    assert!(
        stored.websearch_order.is_empty(),
        "the order entry went with the instance"
    );
}

#[test]
fn a_paste_reaches_the_websearch_dialogs_input() {
    // The regression: a terminal delivers ctrl-v and cmd-v as a
    // bracketed paste event, and the app's paste routing dropped it
    // on the floor for the web search dialog — the key could never
    // land without typing it by hand.
    let mut h = harness_with_secrets(None, None, Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::WebSearch);
    // Enter opens the dialog on the kind list, enter descends to the
    // name line, enter again reaches the key line.
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.websearch_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Enter));
        for _ in 0.."Brave".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "Work key".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Enter));
    }
    h.app.on_paste("sk-pasted-123\n");
    let crate::Screen::Settings(settings) = &h.app.screen else {
        panic!("settings open");
    };
    let dialog = settings.websearch_dialog.as_ref().unwrap();
    assert_eq!(
        dialog.input, "sk-pasted-123",
        "the paste landed, the trailing newline did not"
    );
    // Enter stores it, and the pasted key rides the whole way: the
    // store holds what the dialog carried.
    let effects = h.app.on_key(key(KeyCode::Enter));
    let _ = effects;
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    let stored = h.store.load().unwrap().effective(Some("personal"));
    let id = stored.websearch["Work key"]
        .secret_ref
        .as_ref()
        .expect("the store ref");
    assert_eq!(secrets.get(id).unwrap().as_deref(), Some("sk-pasted-123"));
}

#[test]
fn resume_retags_the_jobs_and_announces_an_interrupted_subagent() {
    use rhema_session_store::{SessionId, SessionStore};

    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let workspace = crate::args::canonical(&h.app.options.workspace);
    let parent_path = crate::sessions::save_session(h.sessions.path(), &workspace, &{
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "the prompt"));
        session
    })
    .unwrap()
    .expect("a spoken session saves");
    let parent_id = parent_path
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    // A child with a working tail: its turn record still says running,
    // and the history ends inside the tool loop.
    let store = rhema_session_store_file::FileStore::scoped(h.sessions.path(), &workspace);
    let child = SessionId::new(format!("{parent_id}.sub-0000000000000002")).unwrap();
    let mut child_session = Session::default();
    rhema_subagents::kind::write_descriptor(
        &mut child_session,
        &rhema_subagents::Descriptor {
            def: "worker".into(),
            prompt: "do the work".into(),
            model: None,
            parent: SessionId::new(parent_id.clone()).unwrap(),
        },
    );
    let queued = rhema_submissions::turn::submit(&mut child_session, "do the work", 1).unwrap();
    rhema_submissions::turn::start(&mut child_session, &queued.id, "do the work", 2).unwrap();
    child_session
        .history
        .push(Message::text(Role::User, "do the work"));
    child_session.history.push(Message {
        annotations: Default::default(),
        role: Role::Assistant,
        content: vec![rhema_core::Content::ToolCall(rhema_core::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "true" }),
            ..Default::default()
        })],
    });
    store.save(&child, &child_session).unwrap();

    h.app.resume_session(&parent_path);
    assert_eq!(h.app.jobs_tag, parent_id, "the poller reads the loaded id");
    let notice = h
        .app
        .transcript
        .lines()
        .iter()
        .find_map(|line| match line {
            Line::Notice(text) if text.contains("interrupted") => Some(text.clone()),
            _ => None,
        })
        .expect("the interrupted subagent is announced");
    assert!(notice.contains(child.as_str()), "{notice}");
    assert!(notice.contains("worker"), "{notice}");

    // Loading starts nothing: the child's tail is untouched, its record
    // is still running, and the parent says interrupted.
    let reloaded = store.load(&child).unwrap().unwrap();
    assert_eq!(reloaded.history, child_session.history);
    let snapshot = rhema_submissions::snapshot(&h.app.session).unwrap();
    let record = snapshot.newest().expect("the parent holds a record");
    assert_eq!(record.kind.as_str(), "subagent");
    assert_eq!(record.status, rhema_submissions::Status::Interrupted);

    // A second load leaves the same record, still interrupted.
    h.app.resume_session(&parent_path);
    let snapshot = rhema_submissions::snapshot(&h.app.session).unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot.newest().unwrap().status,
        rhema_submissions::Status::Interrupted
    );
}

#[test]
fn attaching_a_conversation_moves_the_jobs_tag_and_its_poller() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let start = h.app.jobs_tag.clone();
    let dir = tempfile::tempdir().unwrap();

    let path = dir.path().join("0001794000000000-00.json");
    h.app.adopt_conversation(&path);
    assert_eq!(h.app.jobs_tag, "0001794000000000-00");
    if let Some(jobs) = &h.app.jobs {
        assert_eq!(
            jobs.tag(),
            h.app.jobs_tag,
            "the poller lists the session the shells tag"
        );
    }

    // A name the next resume cannot reproduce leaves the tag alone:
    // moving it would orphan whatever it started.
    h.app
        .adopt_conversation(&dir.path().join("my session.json"));
    assert_eq!(h.app.jobs_tag, "0001794000000000-00");
    assert_ne!(h.app.jobs_tag, start);
}

#[test]
fn a_cleared_conversation_moves_the_jobs_to_its_new_file() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app
        .agent
        .as_mut()
        .unwrap()
        .session_mut()
        .history
        .push(Message::text(Role::User, "the first conversation"));
    h.app.ensure_checkpoint_target();
    let first = h.app.jobs_tag.clone();
    let first_file = h.app.session_file.clone().expect("a session file");

    h.app.clear_session();
    // No job can start before the next turn picks a file, so the tag
    // waits with the conversation.
    assert_eq!(h.app.jobs_tag, first);
    assert!(h.app.session_file.is_none());

    let target = h.app.ensure_checkpoint_target().expect("a fresh file");
    assert_ne!(target, first_file);
    assert_ne!(
        h.app.jobs_tag, first,
        "the new conversation's jobs are its own"
    );
    assert_eq!(
        h.app.jobs_tag,
        crate::jobs::tag_from_path(&target).unwrap(),
        "the tag is the file the turns checkpoint to"
    );
    if let Some(jobs) = &h.app.jobs {
        assert_eq!(jobs.tag(), h.app.jobs_tag);
    }
}

#[test]
fn a_prompt_records_its_turn_before_the_task_takes_the_agent() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut agent = h.app.agent.take().expect("the harness builds an agent");
    let id = h
        .app
        .begin_turn_submission(&mut agent, "fix the test")
        .expect("the turn is recorded");

    let snapshot = rhema_submissions::snapshot(agent.session()).unwrap();
    let record = snapshot.get(&id).expect("the record is in the ledger");
    assert_eq!(record.kind.as_str(), "turn");
    assert_eq!(record.status, rhema_submissions::Status::Running);
    let payload: rhema_submissions::turn::Payload =
        serde_json::from_value(record.payload.clone()).unwrap();
    assert_eq!(payload.input, "fix the test");
    assert_eq!(payload.base, Some(0));

    // The UI thread wrote the record before the task took the agent:
    // a crash between the two has a durable "this turn started".
    let path = h.app.checkpoint_target().expect("a checkpoint target");
    let saved = HostAgent::load_session_from(&path).unwrap();
    let saved_snapshot = rhema_submissions::snapshot(&saved).unwrap();
    assert_eq!(
        saved_snapshot.get(&id).unwrap().status,
        rhema_submissions::Status::Running
    );
}

#[test]
fn a_retry_rearms_the_failed_turns_record() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut agent = h.app.agent.take().expect("the harness builds an agent");
    let id = h
        .app
        .begin_turn_submission(&mut agent, "go")
        .expect("the turn is recorded");
    rhema_submissions::settle(
        agent.session_mut(),
        &id,
        rhema_submissions::Outcome::Failed {
            error: "network error: connection reset".into(),
            retryable: true,
        },
        2,
    )
    .unwrap();

    let retried = h
        .app
        .retry_turn_submission(&mut agent)
        .expect("the failed turn re-arms");
    assert_eq!(retried, id);
    let snapshot = rhema_submissions::snapshot(agent.session()).unwrap();
    let record = snapshot.get(&id).unwrap();
    assert_eq!(record.status, rhema_submissions::Status::Running);
    assert!(record.outcome.is_none(), "the failure is cleared");
    assert_eq!(record.attempts, 1, "a retried failure keeps its count");
}

#[test]
fn a_retry_of_a_record_settled_permanent_runs_unrecorded_with_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut agent = h.app.agent.take().expect("the harness builds an agent");
    let id = h
        .app
        .begin_turn_submission(&mut agent, "go")
        .expect("the turn is recorded");
    // The verdict a driver that was not this build writes: permanent.
    // The ledger refuses the re-arm; the user's retry keeps going.
    rhema_submissions::settle(
        agent.session_mut(),
        &id,
        rhema_submissions::Outcome::Failed {
            error: "rejected key".into(),
            retryable: false,
        },
        2,
    )
    .unwrap();

    assert!(
        h.app.retry_turn_submission(&mut agent).is_none(),
        "the retry runs without a record"
    );
    let notice = h
        .app
        .transcript
        .lines()
        .iter()
        .rev()
        .find_map(|line| match line {
            Line::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(
        notice, "the retry was not recorded",
        "the notice stays the observation, without the record's id and verdict"
    );
    // The refused record is untouched: still failed, still permanent.
    let record = rhema_submissions::snapshot(agent.session())
        .unwrap()
        .get(&id)
        .unwrap()
        .clone();
    assert_eq!(record.status, rhema_submissions::Status::Failed);
}

#[test]
fn a_recovered_finished_turn_is_announced_with_its_answer() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "go", 1).unwrap();
    rhema_submissions::turn::start(&mut session, &record.id, "go", 2).unwrap();
    session.history.push(Message::text(Role::User, "go"));
    session
        .history
        .push(Message::text(Role::Assistant, "the answer"));
    rhema_submissions::settle(
        &mut session,
        &record.id,
        rhema_submissions::Outcome::Done,
        3,
    )
    .unwrap();
    h.app.session = session;

    h.app
        .announce_changes(&[rhema_submissions::Change::Recovered {
            id: record.id,
            kind: rhema_submissions::turn::kind(),
            to: rhema_submissions::Status::Done,
        }]);

    let notice = h
        .app
        .transcript
        .lines()
        .iter()
        .find_map(|line| match line {
            Line::Notice(text) if text.contains("last turn finished") => Some(text.clone()),
            _ => None,
        })
        .expect("the recovered turn is announced");
    assert!(notice.contains("the answer"), "{notice}");
}

#[test]
fn resuming_a_cut_turn_marks_it_interrupted_and_says_so() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let workspace = crate::args::canonical(&h.app.options.workspace);
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "go", 1).unwrap();
    rhema_submissions::turn::start(&mut session, &record.id, "go", 2).unwrap();
    session.history.push(Message::text(Role::User, "go"));
    let path = crate::sessions::save_session(h.sessions.path(), &workspace, &session)
        .unwrap()
        .expect("a spoken session saves");

    h.app.resume_session(&path);

    let snapshot = rhema_submissions::snapshot(&h.app.session).unwrap();
    assert_eq!(
        snapshot.get(&record.id).unwrap().status,
        rhema_submissions::Status::Interrupted
    );
    assert!(
        h.app.transcript.lines().iter().any(
            |line| matches!(line, Line::Notice(text) if text.contains("last turn was interrupted"))
        ),
        "the interrupted turn is announced"
    );
}

#[test]
fn resuming_a_turn_that_finished_after_the_crash_shows_its_answer() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let workspace = crate::args::canonical(&h.app.options.workspace);
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "go", 1).unwrap();
    rhema_submissions::turn::start(&mut session, &record.id, "go", 2).unwrap();
    session.history.push(Message::text(Role::User, "go"));
    session
        .history
        .push(Message::text(Role::Assistant, "the committed answer"));
    let path = crate::sessions::save_session(h.sessions.path(), &workspace, &session)
        .unwrap()
        .expect("a spoken session saves");

    h.app.resume_session(&path);

    let snapshot = rhema_submissions::snapshot(&h.app.session).unwrap();
    assert_eq!(
        snapshot.get(&record.id).unwrap().status,
        rhema_submissions::Status::Done
    );
    assert!(
        h.app.transcript.lines().iter().any(|line| matches!(
            line,
            Line::Notice(text)
                if text.contains("finished while rhema was gone")
                    && text.contains("the committed answer")
        )),
        "the recovered answer is shown"
    );
}

#[test]
fn a_recovered_unfinished_turn_is_announced_as_interrupted() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    let mut session = Session::default();
    let record = rhema_submissions::turn::submit(&mut session, "go", 1).unwrap();
    rhema_submissions::turn::start(&mut session, &record.id, "go", 2).unwrap();
    session.history.push(Message::text(Role::User, "go"));
    h.app.session = session;

    h.app
        .announce_changes(&[rhema_submissions::Change::Recovered {
            id: record.id,
            kind: rhema_submissions::turn::kind(),
            to: rhema_submissions::Status::Interrupted,
        }]);

    let notice = h
        .app
        .transcript
        .lines()
        .iter()
        .find_map(|line| match line {
            Line::Notice(text) if text.contains("interrupted") => Some(text.clone()),
            _ => None,
        })
        .expect("the interrupted turn is announced");
    assert!(notice.contains("last turn"), "{notice}");
}

#[test]
fn a_job_that_ended_while_the_process_was_down_becomes_a_notice() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    h.app.jobs = Some(crate::jobs::JobsHandle::stub(Vec::new()));
    let tag = h.app.jobs_tag.clone();

    // The record the reconcile recovered: a done job for this session.
    let mut session = Session::default();
    let record = rhema_submissions::submit(
        &mut session,
        rhema_submissions::KindName::new("job").unwrap(),
        serde_json::json!({
            "tag": tag,
            "id": "abc",
            "command": "npm run dev",
        }),
        1,
    )
    .unwrap();
    rhema_submissions::start(&mut session, &record.id, 2, None).unwrap();
    let settled = rhema_submissions::settle(
        &mut session,
        &record.id,
        rhema_submissions::Outcome::Done,
        3,
    )
    .unwrap();
    h.app.session = session;

    h.app
        .announce_changes(&[rhema_submissions::Change::Recovered {
            id: settled.id,
            kind: rhema_submissions::KindName::new("job").unwrap(),
            to: rhema_submissions::Status::Done,
        }]);

    let finished = h
        .app
        .jobs
        .as_ref()
        .expect("the stub poller")
        .take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].id, "abc");
    assert_eq!(finished[0].command, "npm run dev");
    assert_eq!(
        finished[0].line,
        "Background job abc (\"npm run dev\") was stopped."
    );
}

#[test]
fn a_job_the_poller_saw_start_becomes_one_running_record() {
    let mut h = harness(Some("k"), Some("m-a"), Session::default());
    assert!(h.app.agent.is_some(), "the harness builds an agent");
    h.app.jobs = Some(crate::jobs::JobsHandle::stub(Vec::new()));
    h.app
        .jobs
        .as_ref()
        .unwrap()
        .note_started(crate::jobs::JobStarted {
            id: "abc".into(),
            command: "npm run dev".into(),
        });

    h.app.drain_submission_writes();
    let snapshot = rhema_submissions::snapshot(h.app.agent.as_ref().unwrap().session()).unwrap();
    assert_eq!(snapshot.len(), 1);
    let record = snapshot.newest().unwrap();
    assert_eq!(record.kind.as_str(), "job");
    assert_eq!(record.status, rhema_submissions::Status::Running);
    let payload: rhema_bash::jobs::kind::Payload =
        serde_json::from_value(record.payload.clone()).unwrap();
    assert_eq!(payload.id, "abc");
    assert_eq!(payload.tag, h.app.jobs.as_ref().unwrap().tag());

    // The queue is empty now: a second drain writes nothing.
    h.app.drain_submission_writes();
    let snapshot = rhema_submissions::snapshot(h.app.agent.as_ref().unwrap().session()).unwrap();
    assert_eq!(snapshot.len(), 1);

    // The poller minted the finish: the record settles without a load.
    h.app
        .jobs
        .as_ref()
        .unwrap()
        .note_finished(crate::jobs::JobFinished {
            id: "abc".into(),
            command: "npm run dev".into(),
            exit_code: Some(0),
            line: "Background job abc finished".into(),
        });
    h.app.drain_submission_writes();
    let snapshot = rhema_submissions::snapshot(h.app.agent.as_ref().unwrap().session()).unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot.newest().unwrap().status,
        rhema_submissions::Status::Done
    );
}

/// A finish the session already delivered settles the subagent's
/// record without waiting for a load: a record left running
/// reconciles at every future load as one the process died before
/// announcing, so a resumed session announces a finish the model
/// already answered, unprompted. The drain that wrote the start
/// settles the delivery, and the same session's reconcile announces
/// nothing.
#[tokio::test]
async fn a_delivered_subagent_finish_settles_before_the_save() {
    use rhema_core::{EventSink, ProviderError};
    use rhema_session_store::SessionId;

    /// Answers one report and ends.
    struct Report;

    #[async_trait::async_trait]
    impl ModelProvider for Report {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<rhema_core::ModelOutput, ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: Message::text(Role::Assistant, "the report"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    let mut h = harness(Some("key"), Some("m"), Session::default());
    let workspace = crate::args::canonical(&h.app.options.workspace);
    let store = rhema_session_store_file::FileStore::scoped(h.sessions.path(), &workspace);
    let parent = SessionId::new(h.app.jobs_tag.clone()).unwrap();
    let tools = {
        let shell = Arc::new(rhema_host_agent::ShellTools::new(std::env::temp_dir()));
        let def = rhema_subagents::SubagentDef {
            name: "scout".into(),
            description: "looks around".into(),
            model: None,
            max_steps: None,
            body: "Look.".into(),
        };
        Arc::new(rhema_subagents::SubagentTools::new(
            Arc::new(Report),
            vec![def],
            shell,
            "m",
        ))
    };
    tools.attach_store(Arc::new(store.clone()), parent.clone());
    h.app.subagents = Some(tools.handle());
    let _ = rhema_core::ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({"name": "scout", "prompt": "look"}),
            ..ToolCall::default()
        },
        &rhema_core::ToolContext::default(),
    )
    .await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !h.app.subagents.as_ref().unwrap().status().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The live finish cards once, the transcript notice the on_tick
    // take would push.
    let events = h.app.subagents.as_ref().unwrap().take_finished();
    assert_eq!(
        events.as_slice(),
        [rhema_subagents::JobFinished {
            id: "job-1".into(),
            name: "scout".into(),
            line: "Subagent scout (job-1) has finished.".into(),
        }],
        "the finish events carry the Scout's line"
    );

    // The drain writes the start record; the notification's delivery
    // is the on_tick idle take.
    h.app.drain_submission_writes();
    let snapshot = rhema_submissions::snapshot(h.app.agent.as_ref().unwrap().session()).unwrap();
    let record = snapshot.newest().expect("the start wrote a record");
    assert_eq!(record.kind.as_str(), "subagent");
    assert_eq!(record.status, rhema_submissions::Status::Running);
    let lines = h.app.subagents.as_ref().unwrap().take_notifications();
    assert_eq!(
        lines,
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );

    // The delivered finish settles on the next drain, the one the exit
    // save follows.
    h.app.drain_submission_writes();
    let snapshot = rhema_submissions::snapshot(h.app.agent.as_ref().unwrap().session()).unwrap();
    let record = snapshot.newest().expect("the record persists");
    assert_eq!(
        record.status,
        rhema_submissions::Status::Done,
        "the delivery settles the record"
    );
    assert!(matches!(
        record.outcome,
        Some(rhema_submissions::Outcome::Done)
    ));

    // The load's reconcile of the settled session finds nothing to
    // announce: a resume does not tell the model about a finish it
    // answered. The finish feed stays empty, the record stays Done.
    let mut kinds = rhema_submissions::Kinds::new().with(Arc::new(rhema_submissions::turn::Turn));
    kinds = kinds.with(Arc::new(rhema_subagents::SubagentKind::new(
        Arc::new(store.clone()) as Arc<dyn rhema_session_store::SessionStore>,
        parent.clone(),
    )));
    let agent = h.app.agent.as_mut().unwrap();
    let changes =
        rhema_submissions::reconcile(agent.session_mut(), &kinds, rhema_submissions::now())
            .unwrap();
    assert!(changes.is_empty(), "the settled record announces nothing");
    let final_events = h.app.subagents.as_ref().unwrap().take_finished();
    assert!(
        final_events.is_empty(),
        "no finish is re-announced: {final_events:?}"
    );
}

#[test]
fn adding_a_decision_model_places_its_key_and_persists() {
    let mut h = harness_with_secrets(None, None, Session::default());
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Decision);
    // The add row is selected on the empty page; enter opens the
    // dialog, the first kind is Jev.
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.decision_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Enter)); // Jev, suggested "Jev"
        for _ in 0.."Jev".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "jev-main".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.stage, crate::screens::DecisionDialogStage::Key);
        for c in "sk-1".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }
    // Enter routes the dialog's Submit through the app: the config
    // write, the store placement, the persist.
    assert_eq!(h.app.on_key(key(KeyCode::Enter)), vec![]);
    let stored = h.store.load().unwrap().effective(Some("personal"));
    let section = stored.decision.as_ref().expect("the section exists");
    let instance = &section.instances["jev-main"];
    assert_eq!(instance.provider, "jev");
    assert_eq!(instance.api_key, None, "the file holds no key");
    let id = instance.secret_ref.as_deref().expect("the store ref");
    assert_eq!(secrets.get(id).unwrap().as_deref(), Some("sk-1"));
    assert_eq!(
        section.instance, None,
        "a first instance needs no default: the first by name serves"
    );
    // The page shows it, default and keyed.
    let crate::Screen::Settings(settings) = &h.app.screen else {
        panic!("settings open");
    };
    assert_eq!(settings.decision.len(), 2, "one instance, then the add row");
    assert!(settings.decision[0].default && settings.decision[0].keyed);
    assert!(
        settings.decision_dialog.is_none(),
        "the submit closed the dialog"
    );
}

#[test]
fn a_clef_decision_model_carries_its_account_and_a_local_one_its_endpoint() {
    let mut h = harness_with_secrets(None, None, Session::default());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Decision);
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.decision_dialog.as_mut().unwrap();
        dialog.handle_key(key(KeyCode::Down)); // Clef
        dialog.handle_key(key(KeyCode::Enter)); // "Clef"
        dialog.handle_key(key(KeyCode::Enter));
        for c in "acct-123".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Enter));
        for c in "cf-token".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }
    h.app.on_key(key(KeyCode::Enter));
    // Second pass: down to the add row, the custom endpoint.
    h.app.on_key(key(KeyCode::Down));
    h.app.on_key(key(KeyCode::Enter));
    {
        let crate::Screen::Settings(settings) = &mut h.app.screen else {
            panic!("settings open");
        };
        let dialog = settings.decision_dialog.as_mut().unwrap();
        for _ in 0..3 {
            dialog.handle_key(key(KeyCode::Down));
        }
        dialog.handle_key(key(KeyCode::Enter)); // "Local Jev"
        dialog.handle_key(key(KeyCode::Enter));
        for c in "http://localhost:30000".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(
            dialog.input, "local",
            "the key line opens on the local default"
        );
    }
    h.app.on_key(key(KeyCode::Enter));
    let stored = h.store.load().unwrap().effective(Some("personal"));
    let section = stored.decision.as_ref().unwrap();
    let clef = &section.instances["Clef"];
    assert_eq!(clef.provider, "clef");
    assert_eq!(clef.account_id.as_deref(), Some("acct-123"));
    assert_eq!(clef.base_url, None);
    let local = &section.instances["Local Jev"];
    assert_eq!(local.provider, "jev", "a custom endpoint is the jev kind");
    assert_eq!(local.base_url.as_deref(), Some("http://localhost:30000"));
    assert_eq!(local.account_id, None, "only Clef carries an account");
}

#[test]
fn enter_on_a_decision_row_sets_the_default_without_a_rebuild() {
    let mut h = harness(None, None, Session::default());
    h.app.config.set_for_profile(Some("personal"), |p| {
        let section = p.decision.get_or_insert_with(Default::default);
        for name in ["Clef", "Jev"] {
            section.instances.insert(
                name.into(),
                rhema_config::DecisionInstance {
                    provider: name.to_lowercase(),
                    api_key: Some("k".into()),
                    secret_ref: None,
                    model: None,
                    account_id: Some("acct".into()),
                    base_url: None,
                },
            );
        }
    });
    let _ = h.store.save(&h.app.config.clone());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Decision);
    // Row 0 is Clef (the first by name, the default); Jev sits below.
    h.app.on_key(key(KeyCode::Down));
    assert_eq!(h.app.on_key(key(KeyCode::Enter)), vec![]);
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert_eq!(
        stored.decision.as_ref().unwrap().instance.as_deref(),
        Some("Jev"),
        "the enter persisted the default"
    );
    let crate::Screen::Settings(settings) = &h.app.screen else {
        panic!("settings open");
    };
    assert!(settings.decision[1].default && !settings.decision[0].default);
}

#[test]
fn removing_the_default_decision_model_clears_it_and_its_store_entry() {
    let mut h = harness_with_secrets(None, None, Session::default());
    let secrets = Arc::clone(h.secrets.as_ref().unwrap());
    h.app.config.set_for_profile(Some("personal"), |p| {
        let section = p.decision.get_or_insert_with(Default::default);
        section.instance = Some("Jev".into());
        section.instances.insert(
            "Jev".into(),
            rhema_config::DecisionInstance {
                provider: "jev".into(),
                api_key: None,
                secret_ref: Some("ref-1".into()),
                model: None,
                account_id: None,
                base_url: None,
            },
        );
        section.instances.insert(
            "Local Jev".into(),
            rhema_config::DecisionInstance {
                provider: "jev".into(),
                api_key: Some("local".into()),
                secret_ref: None,
                model: None,
                account_id: None,
                base_url: Some("http://localhost:30000".into()),
            },
        );
    });
    secrets.set("ref-1", "sk-1").unwrap();
    let _ = h.store.save(&h.app.config.clone());
    h.app
        .open_settings_at(crate::screens::SettingsSection::Decision);
    // Row 0 is Jev, the default: d arms the confirm, y removes it.
    assert_eq!(h.app.on_key(key(KeyCode::Char('d'))), vec![]);
    assert_eq!(h.app.on_key(key(KeyCode::Char('y'))), vec![]);
    let stored = h.store.load().unwrap().effective(Some("personal"));
    let section = stored.decision.as_ref().expect("one instance stays");
    assert!(!section.instances.contains_key("Jev"));
    assert_eq!(
        section.instance, None,
        "the default went with its instance; the next by name serves"
    );
    assert_eq!(
        secrets.get("ref-1").unwrap(),
        None,
        "the store entry went too"
    );
    let crate::Screen::Settings(settings) = &h.app.screen else {
        panic!("settings open");
    };
    assert!(settings.decision[0].default, "Local Jev is the default now");
    // The last one: the section leaves the file.
    assert_eq!(h.app.on_key(key(KeyCode::Char('d'))), vec![]);
    assert_eq!(h.app.on_key(key(KeyCode::Char('y'))), vec![]);
    let stored = h.store.load().unwrap().effective(Some("personal"));
    assert!(
        stored.decision.is_none(),
        "an emptied section leaves the profile"
    );
    let crate::Screen::Settings(settings) = &h.app.screen else {
        panic!("settings open");
    };
    assert_eq!(settings.decision.len(), 1, "the add row alone");
}
