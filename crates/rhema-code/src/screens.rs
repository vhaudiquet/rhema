//! The three screens' input handling and outcome logic — pure state, no
//! terminal. The renderer reads these structs; [`crate::app`] routes their
//! actions into transitions and effects.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_provider::ModelCard;

/// Which backend a login provider row configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProvider {
    /// Google Gemini, by API key.
    Google,
    /// A llama.cpp server, by URL.
    LlamaCpp,
    /// OpenRouter, by API key.
    OpenRouter,
    /// Anthropic, by API key.
    Anthropic,
    /// OpenAI, by API key.
    OpenAi,
    /// DeepSeek, by API key (their OpenAI-compatible endpoint).
    DeepSeek,
    /// GitHub Copilot, through the device-code flow.
    Copilot,
}

/// The provider rows shown at login. Every row configures an implemented
/// backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEntry {
    /// Display name.
    pub name: &'static str,
    /// The backend this row configures.
    pub kind: LoginProvider,
}

/// The login provider list, in display order.
pub const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry {
        name: "Google Gemini",
        kind: LoginProvider::Google,
    },
    ProviderEntry {
        name: "llama.cpp (local server)",
        kind: LoginProvider::LlamaCpp,
    },
    ProviderEntry {
        name: "OpenRouter",
        kind: LoginProvider::OpenRouter,
    },
    ProviderEntry {
        name: "Anthropic",
        kind: LoginProvider::Anthropic,
    },
    ProviderEntry {
        name: "OpenAI",
        kind: LoginProvider::OpenAi,
    },
    ProviderEntry {
        name: "DeepSeek",
        kind: LoginProvider::DeepSeek,
    },
    ProviderEntry {
        name: "GitHub Copilot",
        kind: LoginProvider::Copilot,
    },
];

// --- Profiles ---

/// One row of the profile picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    /// The profile's name.
    pub name: String,
    /// The provider this profile is set to, when configured.
    pub provider: Option<String>,
    /// The model this profile is set to, when configured.
    pub model: Option<String>,
    /// The profile's display color, when configured.
    pub color: Option<String>,
    /// Whether this profile's backend has usable credentials.
    pub usable: bool,
    /// Whether this profile is the one the session is running on.
    pub active: bool,
}

/// What one profile-picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAction {
    /// Nothing to do.
    None,
    /// Switch the session to the named profile.
    Switch(String),
    /// Return to the chat.
    BackToChat,
}

/// The profile picker: one highlighted row, arrows to move, enter to
/// switch, esc back to the chat. Profiles are few, so there is no filter.
#[derive(Debug)]
pub struct ProfileScreen {
    /// The rows, sorted by name.
    pub entries: Vec<ProfileRow>,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl ProfileScreen {
    /// A picker over the config's profiles. `active` marks the row the
    /// session is currently running on.
    pub fn new(profiles: &BTreeMap<String, rhema_config::Profile>, active: Option<&str>) -> Self {
        let mut entries: Vec<ProfileRow> = profiles
            .iter()
            .map(|(name, profile)| ProfileRow {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                color: profile.color.clone(),
                usable: profile.has_credentials(),
                active: Some(name.as_str()) == active,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            entries,
            selected: 0,
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> ProfileAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ProfileAction::None;
        }
        match key.code {
            KeyCode::Down => {
                if !self.entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                }
                ProfileAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ProfileAction::None
            }
            KeyCode::Enter => self.switch(),
            KeyCode::Esc => ProfileAction::BackToChat,
            _ => ProfileAction::None,
        }
    }

    /// Enter: switch to the highlighted profile.
    fn switch(&mut self) -> ProfileAction {
        match self.entries.get(self.selected) {
            Some(row) => ProfileAction::Switch(row.name.clone()),
            None => ProfileAction::None,
        }
    }
}

// --- Resume ---

/// One row of the resume picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    /// The saved session's file; loading it is the app's job.
    pub path: std::path::PathBuf,
    /// The row's age, computed at construction ("3m ago").
    pub age: String,
    /// The session's opening prompt, cut for the row; `None` when the
    /// file could not be read for a preview.
    pub snippet: Option<String>,
}

/// What one resume-picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// Nothing to do.
    None,
    /// Load the saved session at this path.
    Pick(std::path::PathBuf),
    /// Return to the chat.
    BackToChat,
}

/// The resume picker: one highlighted row, arrows to move, enter to
/// load, esc back to the chat. Sessions for one workspace only, newest
/// first — the listing decided that before the screen existed.
#[derive(Debug)]
pub struct ResumeScreen {
    /// The rows, newest first.
    pub entries: Vec<ResumeRow>,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl ResumeScreen {
    /// A picker over the saved sessions; `now` (millis) fixes the age
    /// labels.
    pub fn new(entries: Vec<crate::sessions::SavedSession>, now: u64) -> Self {
        let entries = entries
            .into_iter()
            .map(|saved| ResumeRow {
                path: saved.path,
                age: crate::sessions::ago(saved.saved_at, now),
                snippet: saved.snippet,
            })
            .collect();
        Self {
            entries,
            selected: 0,
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> ResumeAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ResumeAction::None;
        }
        match key.code {
            KeyCode::Down => {
                if !self.entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                }
                ResumeAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ResumeAction::None
            }
            KeyCode::Enter => self.pick(),
            KeyCode::Esc => ResumeAction::BackToChat,
            _ => ResumeAction::None,
        }
    }

    /// Enter: load the highlighted session.
    fn pick(&mut self) -> ResumeAction {
        match self.entries.get(self.selected) {
            Some(row) => ResumeAction::Pick(row.path.clone()),
            None => ResumeAction::None,
        }
    }
}

// --- Slash commands ---

/// The commands whose names start with the slash token in `input`, in
/// table order. `/` alone matches everything; an input that is not a
/// command at all matches nothing. Reads the command table — the single
/// source of truth — so a row exists for the palette the moment it
/// dispatches.
pub fn filtered_commands(input: &str) -> Vec<&'static crate::commands::CommandSpec> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    let needle = input.to_lowercase();
    crate::commands::COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&needle))
        .collect()
}

/// One completable profile row, built from the config registry: the
/// palette reads it, `/profile <name>` switches onto it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteProfile {
    /// The profile's name, the value completion inserts.
    pub name: String,
    /// The provider the profile runs on, when configured.
    pub provider: Option<String>,
    /// The profile's stored model, when configured.
    pub model: Option<String>,
    /// Whether the session currently runs on this profile.
    pub active: bool,
}

/// One row of the input palette: a command while the typed token matches
/// the command table, a model once `/model ` is naming its argument, or
/// a profile under `/profile`. The palette's rendering and its keys
/// (arrows, tab) read the same rows.
#[derive(Debug, Clone)]
pub enum PaletteRow {
    /// A slash command; completing replaces the whole input with its name.
    Command(&'static crate::commands::CommandSpec),
    /// A model under `/model`; completing fills the argument. The
    /// provider's config name prefixes the row and rides on the switch.
    Model {
        /// The provider's config name.
        provider: &'static str,
        /// The model id, the value completion inserts.
        id: String,
        /// What the row says about the model, after the id.
        description: String,
    },
    /// A profile under `/profile`; completing fills the argument.
    Profile {
        /// The profile's name, the value completion inserts.
        name: String,
        /// What the row says about the profile, after the name.
        description: String,
    },
}

/// The palette's rows for an input: command names while the input
/// matches one, the models matching the `/model` argument, or the
/// profiles matching the `/profile` argument. The argument filters
/// follow the picker's rules — the rendered row text, case-insensitive.
pub fn palette_rows(
    input: &str,
    models: &[CatalogModel],
    profiles: &[PaletteProfile],
) -> Vec<PaletteRow> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    if let Some(needle) = input.strip_prefix("/model ") {
        let needle = needle.to_lowercase();
        return models
            .iter()
            .filter(|entry| {
                let mut haystack = format!("{}/{}", entry.provider, entry.model.id);
                if let Some(instance) = &entry.instance {
                    haystack.push_str(&format!(" {instance}"));
                }
                needle.is_empty()
                    || haystack.to_lowercase().contains(&needle)
                    || entry.model.display_name.to_lowercase().contains(&needle)
            })
            .map(|entry| PaletteRow::Model {
                provider: entry.provider,
                id: entry.model.id.clone(),
                description: model_description(&entry.model),
            })
            .collect();
    }
    if let Some(needle) = input.strip_prefix("/profile ") {
        let needle = needle.to_lowercase();
        return profiles
            .iter()
            .filter(|profile| {
                needle.is_empty() || {
                    let row = format!(
                        "{} {} {}",
                        profile.name,
                        profile.provider.as_deref().unwrap_or(""),
                        profile.model.as_deref().unwrap_or("")
                    );
                    row.to_lowercase().contains(&needle)
                }
            })
            .map(|profile| PaletteRow::Profile {
                name: profile.name.clone(),
                description: profile_description(profile),
            })
            .collect();
    }
    filtered_commands(input)
        .into_iter()
        .map(PaletteRow::Command)
        .collect()
}

/// A model's palette description: the display name with the picker's
/// facts — pricing and context window — when the catalog knows them.
fn model_description(model: &ModelCard) -> String {
    let mut detail = model.display_name.clone();
    if let Some(pricing) = model.pricing {
        detail.push_str(&format!(
            " · ${:.2}/M in · ${:.2}/M out",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    if let Some(limit) = model.input_token_limit {
        detail.push_str(&format!(" · {} ctx", format_tokens(Some(limit))));
    }
    detail
}

/// A profile's palette description: its provider and model, the way the
/// profile picker says them, with the active marker last.
fn profile_description(profile: &PaletteProfile) -> String {
    let mut detail = profile
        .provider
        .clone()
        .unwrap_or_else(|| "no provider".into());
    if let Some(model) = &profile.model {
        detail.push_str(&format!(" · {model}"));
    }
    if profile.active {
        detail.push_str(" · active");
    }
    detail
}

/// Humanizes a token count: 1_048_576 → "1.0M", 131_072 → "131k", none →
/// "?".
pub(crate) fn format_tokens(limit: Option<u64>) -> String {
    match limit {
        None => "?".to_owned(),
        Some(limit) if limit >= 1_000_000 => {
            format!("{:.1}M", limit as f64 / 1_000_000.0)
        }
        Some(limit) if limit >= 10_000 => format!("{}k", limit / 1_000),
        Some(limit) => format!("{limit}"),
    }
}

// --- Login ---

/// What Esc means on the login screen: with no session behind it, leaving
/// the login leaves the app; opened from the chat with `/login`, it goes
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginPurpose {
    /// Startup without a usable backend: Esc quits.
    Initial,
    /// Opened from the chat via `/login`: Esc returns to the chat, and a
    /// successful login lands in a picker that also knows the way back.
    FromChat,
}

/// Which half of the two-stage login is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStage {
    /// The profile's logged-in instances plus the add row: enter
    /// activates, `d` forgets, enter on the add row starts a new login.
    Instances,
    /// The provider list for a new instance: arrows to choose, enter to
    /// continue.
    SelectProvider,
    /// The name input for the instance being added.
    NameInstance,
    /// The credential input(s) for the chosen provider — llama.cpp and
    /// OpenAI ask a URL first, then a key.
    EnterCredential,
    /// An interactive flow (device code and friends) is running: its
    /// instruction and progress render from the screen's `flow_*`
    /// fields, Esc cancels, and the credential arrives through the
    /// app — the screen never sees the protocol.
    Flow,
}

/// Where the login screen stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    /// Waiting for input.
    Idle,
    /// A key was submitted; validation is in flight. Enter is inert until
    /// the outcome arrives.
    Validating,
    /// The last validation failed; the text is the provider's error.
    Error(String),
}

/// One row of the instance list: a stored instance, or the add row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow<'a> {
    /// The instance's name (empty on the add row).
    pub name: &'a str,
    /// The provider config name (empty on the add row).
    pub provider: &'a str,
    /// Whether this instance's backend serves the profile.
    pub active: bool,
    /// Whether this is the add-a-new-instance row.
    pub add: bool,
}

/// What one login keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAction {
    /// Nothing to do.
    None,
    /// The highlighted instance becomes the profile's active one.
    Activate,
    /// The highlighted instance is forgotten.
    Remove,
    /// Validate the entered key against the provider.
    Validate,
    /// The highlighted provider logs in through an interactive flow:
    /// spawn it (the stage transition already happened on the screen).
    StartFlow,
    /// Cancel the running flow; the app drops its task handle.
    CancelFlow,
    /// Leave the app (no key to log in with, nothing else to show).
    Quit,
}

/// The login screen: profile tabs, the logged-in instance list, and the
/// add-instance flow (provider, name, key or server URL), plus the
/// validation status.
#[derive(Debug)]
pub struct LoginScreen {
    /// Which half of the two-stage login is showing.
    pub stage: LoginStage,
    /// The highlighted row: an instance on the `Instances` stage, a
    /// provider on `SelectProvider`.
    pub selected: usize,
    /// The line being typed: the instance name on `NameInstance`, the
    /// credential on `EnterCredential`.
    pub input: String,
    /// Current status.
    pub status: LoginStatus,
    /// What Esc does here.
    pub purpose: LoginPurpose,
    /// The registry of profiles; the tabs cycle through its keys.
    pub profiles: BTreeMap<String, rhema_config::Profile>,
    /// The profile this login writes into — the selected tab.
    pub profile: String,
    /// The stored settings of that profile: the instance list and what
    /// each provider row reports as registered.
    pub stored: rhema_config::Profile,
    /// The instance being added, from the provider pick through
    /// validation.
    pub pending: Option<PendingInstance>,
    /// The credential stage's second prompt: llama.cpp and openai ask a
    /// URL first, then a key — when set, the input line is that key.
    pub second_input: bool,
    /// The flow's instruction, once the running flow has reported one
    /// (the Flow stage renders from this).
    pub flow_instruction: Option<rhema_provider::LoginInstruction>,
    /// The flow's latest progress note, when it has sent one.
    pub flow_waiting: Option<String>,
}

/// The instance the add flow is collecting: the provider kind, the name
/// it will land under, and the URL captured on the credential stage's
/// first prompt (llama.cpp's server address, OpenAI-compatible base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInstance {
    /// The provider kind being logged into.
    pub provider: LoginProvider,
    /// The name the instance will carry in the profile.
    pub name: String,
    /// The URL from the first credential prompt, when the provider asks
    /// for one.
    pub base_url: Option<String>,
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginScreen {
    /// A fresh login screen: the instance list, nothing highlighted
    /// beyond the first row, Esc quits.
    pub fn new() -> Self {
        Self {
            input: String::new(),
            selected: 0,
            stage: LoginStage::Instances,
            status: LoginStatus::Idle,
            purpose: LoginPurpose::Initial,
            profiles: BTreeMap::new(),
            profile: "personal".to_owned(),
            stored: rhema_config::Profile::default(),
            pending: None,
            second_input: false,
            flow_instruction: None,
            flow_waiting: None,
        }
    }

    /// Sets the profile registry (the tabs) and carries the stored
    /// settings of the initially selected tab.
    pub fn with_registry(
        self,
        profiles: &BTreeMap<String, rhema_config::Profile>,
        initial: &str,
    ) -> Self {
        let stored = profiles.get(initial).cloned().unwrap_or_default();
        Self {
            profiles: profiles.clone(),
            profile: initial.to_owned(),
            stored,
            ..self
        }
    }

    /// The names of the profile tabs, sorted.
    pub fn tab_names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    /// Switches to a profile tab and carries its stored settings.
    pub fn select_profile(&mut self, name: &str) {
        self.profile = name.to_owned();
        self.stored = self.profiles.get(name).cloned().unwrap_or_default();
    }

    /// A login screen opened from the chat: Esc returns there.
    pub fn for_chat() -> Self {
        Self {
            purpose: LoginPurpose::FromChat,
            ..Self::new()
        }
    }

    /// The hint under the input, per the stage and what Esc does.
    pub fn input_hint(&self) -> String {
        let base = match self.stage {
            LoginStage::Instances => {
                "enter uses it · d forgets · enter on + adds a login".to_owned()
            }
            LoginStage::SelectProvider => "arrows choose · enter to log in".to_owned(),
            LoginStage::NameInstance => "name this login · enter to continue".to_owned(),
            LoginStage::EnterCredential => {
                let what = match (self.pending.as_ref().map(|p| p.provider), self.second_input) {
                    (Some(LoginProvider::LlamaCpp), false) => "the server URL",
                    (Some(LoginProvider::LlamaCpp), true) => "the bearer key (empty for none)",
                    (Some(LoginProvider::OpenAi), false) => {
                        "the base URL (empty for api.openai.com)"
                    }
                    _ => "the API key",
                };
                format!("enter {what} · ctrl-v paste · enter when done")
            }
            LoginStage::Flow => "esc cancels the login".to_owned(),
        };
        match self.purpose {
            LoginPurpose::FromChat => format!("{base} · esc back"),
            LoginPurpose::Initial => base,
        }
    }

    /// The backend the highlighted row configures, on the provider list.
    pub fn selected_provider(&self) -> LoginProvider {
        PROVIDERS[self.selected.min(PROVIDERS.len() - 1)].kind
    }

    /// The instance list as rendered rows: the stored instances in name
    /// order, then the add row. The flag marks the instance whose
    /// backend serves the profile.
    pub fn instance_rows(&self) -> Vec<InstanceRow<'_>> {
        let active = self.stored.active_instance().map(|(name, _)| name);
        let mut rows: Vec<InstanceRow<'_>> = self
            .stored
            .instance_entries()
            .into_iter()
            .map(|(name, instance)| InstanceRow {
                name,
                provider: instance.provider.as_str(),
                active: active == Some(name),
                add: false,
            })
            .collect();
        rows.push(InstanceRow {
            name: "",
            provider: "",
            active: false,
            add: true,
        });
        rows
    }

    /// The name of the highlighted instance row; `None` on the add row.
    pub fn selected_instance(&self) -> Option<&str> {
        if self.stage != LoginStage::Instances {
            return None;
        }
        self.instance_rows()
            .get(self.selected)
            .and_then(|row| (!row.add).then_some(row.name))
    }

    /// The input's label, per the stage's prompt.
    pub fn input_label(&self) -> &'static str {
        match self.stage {
            LoginStage::NameInstance => "name: ",
            LoginStage::EnterCredential => {
                match (self.pending.as_ref().map(|p| p.provider), self.second_input) {
                    (Some(LoginProvider::LlamaCpp), false) => "server URL: ",
                    (Some(LoginProvider::OpenAi), false) => "base URL: ",
                    _ => "API key: ",
                }
            }
            _ => "",
        }
    }

    /// The rendered echo of the input: secrets mask to bullets (no key
    /// material ever rendered); a URL or an instance name is not secret,
    /// so it shows.
    pub fn masked_input(&self) -> String {
        let secret = match self.stage {
            LoginStage::NameInstance => false,
            LoginStage::EnterCredential => match self.pending.as_ref().map(|p| p.provider) {
                Some(LoginProvider::LlamaCpp) => self.second_input,
                Some(LoginProvider::OpenAi) => self.second_input,
                _ => true,
            },
            _ => true,
        };
        if secret {
            "\u{2022}".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        }
    }

    /// A bracketed paste landed on a text input. A name, a key, or a
    /// server URL is a single line: control characters — newlines above
    /// all, the trailing one a stray copy loves to carry — are dropped.
    pub fn paste(&mut self, text: &str) {
        if matches!(
            self.stage,
            LoginStage::NameInstance | LoginStage::EnterCredential
        ) {
            self.input.extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> LoginAction {
        // ctrl-v and ctrl-shift-v are paste in terminals that leave them
        // unbound: the key reads the clipboard, carrying the text a
        // bracketed paste would have. Only on the text lines.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                self.stage,
                LoginStage::NameInstance | LoginStage::EnterCredential
            )
        {
            if let Some(text) = crate::clipboard::read() {
                self.paste(&text);
            }
            return LoginAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return LoginAction::None;
        }
        match self.stage {
            LoginStage::Instances => self.handle_instance_key(key),
            LoginStage::SelectProvider => self.handle_provider_key(key),
            LoginStage::NameInstance => self.handle_name_key(key),
            LoginStage::EnterCredential => self.handle_credential_key(key),
            LoginStage::Flow => self.handle_flow_key(key),
        }
    }

    /// Keys on the instance list: arrows choose, enter activates — or
    /// descends into the add flow on the add row — `d` forgets, the
    /// tabs cycle profiles, esc leaves the login.
    fn handle_instance_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let count = self.instance_rows().len();
                self.move_selection(key.code == KeyCode::Down, count);
                LoginAction::None
            }
            KeyCode::Enter => {
                if self.selected_instance().is_some() {
                    LoginAction::Activate
                } else {
                    self.input.clear();
                    self.status = LoginStatus::Idle;
                    self.second_input = false;
                    self.pending = None;
                    self.selected = 0;
                    self.stage = LoginStage::SelectProvider;
                    LoginAction::None
                }
            }
            KeyCode::Char('d') if self.selected_instance().is_some() => LoginAction::Remove,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.cycle_profile_tab(key.code == KeyCode::Right || key.code == KeyCode::Tab);
                LoginAction::None
            }
            KeyCode::Esc => LoginAction::Quit,
            _ => LoginAction::None,
        }
    }

    /// Keys on the provider list: arrows choose, enter descends into the
    /// name input, esc returns to the instance list.
    fn handle_provider_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_selection(key.code == KeyCode::Down, PROVIDERS.len());
                LoginAction::None
            }
            KeyCode::Enter => {
                let provider = self.selected_provider();
                self.pending = Some(PendingInstance {
                    provider,
                    name: self.name_suggestion(provider),
                    base_url: None,
                });
                self.input = self.pending_name();
                self.status = LoginStatus::Idle;
                self.stage = LoginStage::NameInstance;
                LoginAction::None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.cycle_profile_tab(key.code == KeyCode::Right || key.code == KeyCode::Tab);
                LoginAction::None
            }
            KeyCode::Esc => {
                self.status = LoginStatus::Idle;
                self.selected = 0;
                self.stage = LoginStage::Instances;
                LoginAction::None
            }
            _ => LoginAction::None,
        }
    }

    /// Keys while naming the instance: enter accepts a non-blank name and
    /// descends into the credential input — or starts the provider's
    /// interactive flow, for the providers that log in that way — esc
    /// returns to the provider list.
    fn handle_name_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                LoginAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                LoginAction::None
            }
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    return LoginAction::None;
                }
                if let Some(pending) = self.pending.as_mut() {
                    pending.name = self.input.trim().to_owned();
                }
                self.input.clear();
                if self.pending.as_ref().map(|p| p.provider) == Some(LoginProvider::Copilot) {
                    // A flow provider has no key to type: the stage
                    // flips and the app spawns the flow.
                    self.status = LoginStatus::Validating;
                    self.flow_instruction = None;
                    self.flow_waiting = None;
                    self.stage = LoginStage::Flow;
                    return LoginAction::StartFlow;
                }
                self.second_input = false;
                self.stage = LoginStage::EnterCredential;
                LoginAction::None
            }
            KeyCode::Esc => {
                self.input.clear();
                self.status = LoginStatus::Idle;
                self.pending = None;
                self.stage = LoginStage::SelectProvider;
                LoginAction::None
            }
            _ => LoginAction::None,
        }
    }

    /// Keys while typing the credential: enter validates — after
    /// capturing the first prompt's URL for the providers that ask one —
    /// esc returns to the name input.
    fn handle_credential_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                LoginAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                LoginAction::None
            }
            KeyCode::Enter => {
                let provider = self.pending.as_ref().map(|p| p.provider);
                if !self.second_input
                    && matches!(
                        provider,
                        Some(LoginProvider::LlamaCpp | LoginProvider::OpenAi)
                    )
                {
                    // The URL prompt: captured, then the key is asked.
                    if let Some(pending) = self.pending.as_mut() {
                        pending.base_url = Some(self.input.trim().to_owned());
                    }
                    self.second_input = true;
                    self.input.clear();
                    self.status = LoginStatus::Idle;
                    return LoginAction::None;
                }
                self.submit()
            }
            KeyCode::Esc => {
                self.input = self.pending_name();
                self.second_input = false;
                self.status = LoginStatus::Idle;
                self.stage = LoginStage::NameInstance;
                LoginAction::None
            }
            _ => LoginAction::None,
        }
    }

    /// Keys while a flow runs: esc cancels back to the instance list;
    /// everything else is inert — the flow has no input of its own.
    fn handle_flow_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Esc => {
                self.status = LoginStatus::Idle;
                self.flow_instruction = None;
                self.flow_waiting = None;
                self.pending = None;
                self.second_input = false;
                self.selected = 0;
                self.stage = LoginStage::Instances;
                LoginAction::CancelFlow
            }
            _ => LoginAction::None,
        }
    }

    /// The name the pending instance will land under; empty outside the
    /// add flow.
    fn pending_name(&self) -> String {
        self.pending
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// A deduped default name for a new instance of `provider`: the
    /// provider's own name, numbered when taken.
    fn name_suggestion(&self, provider: LoginProvider) -> String {
        let base = match provider {
            LoginProvider::Google => "google",
            LoginProvider::LlamaCpp => "llama-cpp",
            LoginProvider::OpenRouter => "openrouter",
            LoginProvider::Anthropic => "anthropic",
            LoginProvider::OpenAi => "openai",
            LoginProvider::DeepSeek => "deepseek",
            LoginProvider::Copilot => "copilot",
        };
        if !self.stored.instances.contains_key(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !self.stored.instances.contains_key(name))
            .unwrap_or_else(|| format!("{base}-{}", self.stored.instances.len() + 1))
    }

    /// Cycles the profile tab.
    fn cycle_profile_tab(&mut self, forward: bool) {
        let names: Vec<String> = self.profiles.keys().cloned().collect();
        if names.is_empty() {
            return;
        }
        let current = names
            .iter()
            .position(|name| *name == self.profile)
            .unwrap_or(0);
        let count = names.len() as isize;
        let step = if forward { 1 } else { -1 };
        let next = (current as isize + step).rem_euclid(count) as usize;
        self.select_profile(&names[next]);
        // The row count moves with the tab: restart at the top.
        self.selected = 0;
    }

    /// Moves the selection among `count` rows, wrapping.
    fn move_selection(&mut self, down: bool, count: usize) {
        let count = count.max(1) as isize;
        let step = if down { 1 } else { -1 };
        self.selected = ((self.selected as isize + step).rem_euclid(count)) as usize;
    }

    /// Enter on the final credential prompt: validate, unless a
    /// validation is already in flight. A llama.cpp bearer key may be
    /// blank (an open server); every other key must be filled.
    fn submit(&mut self) -> LoginAction {
        if matches!(self.status, LoginStatus::Validating) {
            return LoginAction::None;
        }
        let blank = self.input.trim().is_empty();
        let key_optional = self.second_input
            && self.pending.as_ref().map(|p| p.provider) == Some(LoginProvider::LlamaCpp);
        if blank && !key_optional {
            return LoginAction::None;
        }
        self.status = LoginStatus::Validating;
        LoginAction::Validate
    }

    /// Records the validation outcome: an error renders inline and allows
    /// retrying; success leaves the transition to the app (it needs the
    /// models and the store).
    pub fn on_validated(&mut self, result: &Result<Vec<ModelCard>, String>) {
        if let Err(message) = result {
            self.status = LoginStatus::Error(message.clone());
        }
    }
}

// --- Model picker ---

/// Where the picker stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerStatus {
    /// The model list is being fetched.
    Loading,
    /// Models are available.
    Ready,
    /// The fetch failed; the text is the provider's error. `r` retries.
    Error(String),
}

/// What the picker returns to when dismissed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPurpose {
    /// No model is chosen yet; Esc quits the app.
    Initial,
    /// Opened mid-session with Ctrl-P; Esc returns to the chat.
    FromChat,
}

/// What one picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    /// Nothing to do.
    None,
    /// The highlighted model was chosen, with its provider's config name
    /// and the instance it came from: a pick from another provider or
    /// instance switches the session's backend.
    Choose {
        /// The provider's config name.
        provider: &'static str,
        /// The instance's name in the profile.
        instance: Option<String>,
        /// The model's id.
        id: String,
    },
    /// Leave the app.
    Quit,
    /// Return to the chat.
    BackToChat,
    /// Retry the failed model fetch.
    Retry,
}

/// One pickable model, tagged with its provider's config name and the
/// instance it came from: the row prefix, the filter's provider match,
/// and the switch target on choose. A profile may hold several logins —
/// including several of one provider — so a catalog spans both.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    /// The provider's config name (`google`, `openrouter`, ...).
    pub provider: &'static str,
    /// The instance's name in the profile's registry.
    pub instance: Option<String>,
    /// The normalized model.
    pub model: ModelCard,
}

/// The model picker: text chat models across every logged-in provider,
/// sorted by provider then id, a type-to-filter prompt, and one
/// highlighted row.
#[derive(Debug)]
pub struct PickerScreen {
    /// The models to pick from (text chat only).
    pub models: Vec<CatalogModel>,
    /// Providers whose fetch failed while others succeeded; the footer
    /// names them so a missing catalog is never silent.
    pub failed: Vec<&'static str>,
    /// The filter text; printable keys extend it without any prefix.
    pub filter: String,
    /// Index into the filtered list of the highlighted row.
    pub selected: usize,
    /// Current status.
    pub status: PickerStatus,
    /// What Esc does here.
    pub purpose: PickerPurpose,
}

impl PickerScreen {
    /// A picker over `models` (text chat models only, sorted by provider
    /// then id), initially unfiltered.
    pub fn new(mut models: Vec<CatalogModel>, purpose: PickerPurpose) -> Self {
        models.retain(|entry| Self::usable(&entry.model));
        models.sort_by(|a, b| a.provider.cmp(b.provider).then(a.model.id.cmp(&b.model.id)));
        Self {
            models,
            failed: Vec::new(),
            filter: String::new(),
            selected: 0,
            status: PickerStatus::Ready,
            purpose,
        }
    }

    /// The models this coding agent can drive: chat-capable rows. The
    /// adapters only hand out text chat models — the id markers that
    /// decide "text" are per-provider catalog knowledge — so this is just
    /// the defensive line against a row without the chat marker.
    fn usable(model: &ModelCard) -> bool {
        model.supports_chat()
    }

    /// Replaces the model list (scoped and re-sorted) and clamps the
    /// selection.
    pub fn set_models(&mut self, models: Vec<CatalogModel>) {
        let mut models = models;
        models.retain(|entry| Self::usable(&entry.model));
        self.models = models;
        self.models
            .sort_by(|a, b| a.provider.cmp(b.provider).then(a.model.id.cmp(&b.model.id)));
        self.clamp_selection();
    }

    /// The models matching the filter: case-insensitive substring on the
    /// rendered row — `provider/id`, so `openrouter/` names the provider's
    /// whole catalog and `openrouter/anthropic` narrows inside it — or on
    /// the display name, in the sorted order.
    pub fn filtered(&self) -> Vec<&CatalogModel> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|entry| {
                let mut haystack = format!("{}/{}", entry.provider, entry.model.id);
                if let Some(instance) = &entry.instance {
                    haystack.push_str(&format!(" {instance}"));
                }
                needle.is_empty()
                    || haystack.to_lowercase().contains(&needle)
                    || entry.model.display_name.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn clamp_selection(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Handles one keypress. Typing filters by default: printable keys
    /// extend the filter, arrows navigate, enter picks. Esc clears a
    /// non-empty filter first and only then quits or returns to the chat.
    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return PickerAction::None;
        }
        if self.status == PickerStatus::Ready {
            match key.code {
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.clamp_selection();
                    return PickerAction::None;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.clamp_selection();
                    return PickerAction::None;
                }
                KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    return PickerAction::None;
                }
                KeyCode::Down => {
                    self.selected =
                        (self.selected + 1).min(self.filtered().len().saturating_sub(1));
                    return PickerAction::None;
                }
                KeyCode::Enter => return self.choose(),
                KeyCode::Esc => {
                    if !self.filter.is_empty() {
                        self.filter.clear();
                        self.clamp_selection();
                        return PickerAction::None;
                    }
                    return match self.purpose {
                        PickerPurpose::Initial => PickerAction::Quit,
                        PickerPurpose::FromChat => PickerAction::BackToChat,
                    };
                }
                _ => return PickerAction::None,
            }
        }
        // Loading or failed: only retry and leave are meaningful.
        match key.code {
            KeyCode::Char('r') if matches!(self.status, PickerStatus::Error(_)) => {
                self.status = PickerStatus::Loading;
                PickerAction::Retry
            }
            KeyCode::Esc => match self.purpose {
                PickerPurpose::Initial => PickerAction::Quit,
                PickerPurpose::FromChat => PickerAction::BackToChat,
            },
            _ => PickerAction::None,
        }
    }

    /// Enter: choose the highlighted model; an empty list chooses nothing.
    fn choose(&mut self) -> PickerAction {
        if self.status != PickerStatus::Ready {
            return PickerAction::None;
        }
        match self.filtered().get(self.selected) {
            Some(entry) => PickerAction::Choose {
                provider: entry.provider,
                instance: entry.instance.clone(),
                id: entry.model.id.clone(),
            },
            None => PickerAction::None,
        }
    }
}

// --- Chat input ---

/// What one chat keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAction {
    /// Nothing to do.
    None,
    /// Enter on the (possibly empty) input; the app drops empty and
    /// suppresses submits while a turn runs.
    Submit(String),
}

/// The chat screen's input-box state: text, cursor (character index), and
/// the transcript scroll offset (rows kept above the bottom).
///
/// The transcript word-wraps, so scroll counts *rendered rows*, not
/// logical lines; render clamps it to the content height.
#[derive(Debug, Default)]
pub struct ChatState {
    /// The input being typed. An [`Self::MARK`] character anchors an
    /// attachment chip at the position its paste landed.
    pub input: String,
    /// Paste payloads, in the order their [`Self::MARK`] anchors appear
    /// in `input` — a 1:1 pairing the edit paths keep true. Chips render
    /// at their anchors; submit expands the payloads in place.
    pub attachments: Vec<String>,
    /// Cursor position as a character index into `input`.
    pub cursor: usize,
    /// Transcript scroll: rendered rows scrolled up beyond the bottom
    /// (0 follows the latest line).
    pub scroll: usize,
    /// Highlighted row of the command palette while a slash input filters
    /// it; meaningless otherwise.
    pub palette_index: usize,
}

impl ChatState {
    /// Rows per PageUp/PageDown press — one mouse-wheel tick, so both
    /// controls step the history identically.
    pub const SCROLL_STEP: usize = 3;
    /// A paste at or under this many lines joins the input inline;
    /// longer ones become attachments.
    pub const PASTE_INLINE_LINES: usize = 3;
    /// The object-replacement character: one attachment anchor in
    /// `input`. It is one cursor step, and backspace or delete on it
    /// removes the chip and its payload together.
    pub const MARK: char = '\u{FFFC}';

    /// A fresh input box.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scrolls the transcript by `rows` (positive looks back); used by the
    /// mouse wheel as well as the page keys.
    pub fn scroll_by(&mut self, rows: isize) {
        if rows >= 0 {
            self.scroll = self.scroll.saturating_add(rows as usize);
        } else {
            self.scroll = self.scroll.saturating_sub((-rows) as usize);
        }
    }

    /// Inserts a newline at the cursor: the input grows a line instead
    /// of submitting.
    pub fn insert_newline(&mut self) {
        self.insert_str("\n");
    }

    /// A paste landed. Short enough, it joins the input inline; longer,
    /// it becomes an attachment anchored at the cursor — one chip in the
    /// box at the paste's place, full content on submit.
    pub fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.lines().count() > Self::PASTE_INLINE_LINES {
            self.attachments.push(normalized);
            self.insert_char(Self::MARK);
        } else {
            self.insert_str(&normalized);
        }
    }

    /// Inserts one character at the cursor.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.byte_offset(), c);
        self.cursor += 1;
        self.palette_index = 0;
    }

    /// Removes `input[start..end]` (char offsets) and, with it, the
    /// payload of every attachment anchor in the range — the 1:1
    /// mark-to-payload pairing survives any deletion.
    fn delete_range(&mut self, start: usize, end: usize) {
        let dropped = self.input[start..end].matches(Self::MARK).count();
        if dropped > 0 {
            let first = self.input[..start].matches(Self::MARK).count();
            self.attachments.drain(first..first + dropped);
        }
        self.input.replace_range(start..end, "");
    }

    /// The text a submit sends: the typed input with every attachment's
    /// full content expanded at its anchor — the paste lands where it
    /// was made.
    fn submission(&self) -> String {
        let mut full = String::new();
        let mut payloads = self.attachments.iter();
        for c in self.input.chars() {
            if c == Self::MARK {
                full.push_str(payloads.next().map(String::as_str).unwrap_or_default());
            } else {
                full.push(c);
            }
        }
        full
    }

    /// Inserts `text` at the cursor, newlines and all — the bracketed
    /// paste path. Carriage returns normalize to newlines, since a
    /// terminal paste may carry either.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let count = normalized.chars().count();
        self.input.insert_str(self.byte_offset(), &normalized);
        self.cursor += count;
        self.palette_index = 0;
    }

    /// The cursor's logical line index, 0-based — the input window's
    /// anchor.
    pub fn cursor_line_index(&self) -> usize {
        self.input
            .chars()
            .take(self.cursor)
            .filter(|c| *c == '\n')
            .count()
    }

    /// The cursor's column within its logical line.
    pub fn cursor_column(&self) -> usize {
        let (start, _) = self.cursor_line();
        self.cursor - start
    }

    /// The cursor's logical line as `(start, char_len)`, newline
    /// characters excluded from the length.
    fn cursor_line(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let pos = self.cursor.min(chars.len());
        let mut start = 0;
        for (index, c) in chars.iter().enumerate() {
            if index == pos {
                break;
            }
            if *c == '\n' {
                start = index + 1;
            }
        }
        let end = chars[pos..]
            .iter()
            .position(|c| *c == '\n')
            .map(|offset| pos + offset)
            .unwrap_or(chars.len());
        (start, end - start)
    }

    /// Moves the cursor one logical line up or down, keeping the column
    /// where the target line is long enough.
    fn move_vertical(&mut self, up: bool) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut bounds: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (index, c) in chars.iter().enumerate() {
            if *c == '\n' {
                bounds.push((start, index - start));
                start = index + 1;
            }
        }
        bounds.push((start, chars.len() - start));
        let line = bounds
            .iter()
            .position(|(s, l)| self.cursor >= *s && self.cursor <= s + l)
            .unwrap_or(bounds.len() - 1);
        let target = if up {
            line.checked_sub(1)
        } else {
            (line + 1 < bounds.len()).then_some(line + 1)
        };
        if let Some(t) = target {
            let (start, len) = bounds[t];
            self.cursor = start + (self.cursor - bounds[line].0).min(len);
        }
    }

    /// Byte offset of the cursor inside `input` (a char boundary).
    fn byte_offset(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// Handles one keypress. Ctrl combos are the app's (quit, model
    /// picker); Esc is the app's (approval decline / quit confirm). The
    /// model catalog and profile registry feed the `/model` and
    /// `/profile` palette rows; empty slices simply leave those rows
    /// empty.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        models: &[CatalogModel],
        profiles: &[PaletteProfile],
    ) -> ChatAction {
        // Ctrl-J is readline's historical newline: it inserts even
        // though plain control combos are inert.
        if key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL {
            self.insert_newline();
            return ChatAction::None;
        }
        // Ctrl-v (with or without shift) is paste in terminals that do
        // not bind it themselves: the clipboard is read directly, the
        // same text a bracketed paste would have carried — attachments
        // and all.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            if let Some(text) = crate::clipboard::read() {
                self.paste(&text);
            }
            return ChatAction::None;
        }
        // AltGr lands as CONTROL|ALT on most terminals outside the kitty
        // protocol: a printable char carrying both is composed text, not
        // a combo — on French AZERTY, `@` is AltGr+0.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !(key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char(_)))
        {
            return ChatAction::None;
        }
        // Shift-enter reports as a distinct key only under the kitty
        // keyboard protocol; alt-enter and ctrl-j are the universal
        // newline keys.
        if key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.insert_newline();
            return ChatAction::None;
        }
        match key.code {
            KeyCode::Char(c) => {
                // A terminal reporting every key but omitting the
                // shifted-alternate field leaves SHIFT on an unshifted
                // char; the case is ours to fix then.
                let c = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                self.input.insert(self.byte_offset(), c);
                self.cursor += 1;
                self.palette_index = 0;
                ChatAction::None
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let end = self.byte_offset();
                    self.cursor -= 1;
                    let start = self.byte_offset();
                    self.delete_range(start, end);
                    self.palette_index = 0;
                }
                ChatAction::None
            }
            KeyCode::Delete => {
                let start = self.byte_offset();
                if start < self.input.len() {
                    self.cursor += 1;
                    let end = self.byte_offset();
                    self.cursor -= 1;
                    self.delete_range(start, end);
                }
                ChatAction::None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                ChatAction::None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                ChatAction::None
            }
            KeyCode::Home => {
                let (start, _) = self.cursor_line();
                self.cursor = start;
                ChatAction::None
            }
            KeyCode::End => {
                let (start, len) = self.cursor_line();
                self.cursor = start + len;
                ChatAction::None
            }
            // The palette owns the vertical arrows while a slash input
            // filters it — commands, models under `/model`, profiles
            // under `/profile`.
            KeyCode::Up | KeyCode::Down
                if !palette_rows(&self.input, models, profiles).is_empty() =>
            {
                let count = palette_rows(&self.input, models, profiles).len();
                if key.code == KeyCode::Down {
                    self.palette_index = (self.palette_index + 1).min(count - 1);
                } else {
                    self.palette_index = self.palette_index.saturating_sub(1);
                }
                ChatAction::None
            }
            // A multi-line input hands the vertical arrows to the cursor.
            KeyCode::Up if self.input.contains('\n') => {
                self.move_vertical(true);
                ChatAction::None
            }
            KeyCode::Down if self.input.contains('\n') => {
                self.move_vertical(false);
                ChatAction::None
            }
            KeyCode::Tab => {
                let rows = palette_rows(&self.input, models, profiles);
                if let Some(row) = rows.get(self.palette_index.min(rows.len().saturating_sub(1))) {
                    // The input is replaced wholesale: any anchors in it
                    // go, and their payloads go with them.
                    self.attachments.clear();
                    match row {
                        PaletteRow::Command(spec) => {
                            self.input = spec.name.to_owned();
                        }
                        PaletteRow::Model { id, .. } => {
                            self.input = format!("/model {id}");
                        }
                        PaletteRow::Profile { name, .. } => {
                            self.input = format!("/profile {name}");
                        }
                    }
                    self.cursor = self.input.chars().count();
                }
                ChatAction::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(Self::SCROLL_STEP);
                ChatAction::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(Self::SCROLL_STEP);
                ChatAction::None
            }
            KeyCode::Enter => {
                // A new prompt goes to the live tail; stop pinning
                // history. The input is NOT cleared: the app clears when
                // it accepts the submit, so a declined one keeps it.
                self.scroll = 0;
                ChatAction::Submit(self.submission())
            }
            _ => ChatAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_provider::ModelCard;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn model(id: &str, display: &str, chat: bool) -> ModelCard {
        ModelCard {
            id: id.into(),
            display_name: display.into(),
            description: String::new(),
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            generation_methods: if chat {
                vec!["generateContent".into()]
            } else {
                vec!["generateAudio".into()]
            },
            pricing: None,
        }
    }

    // --- Slash commands ---

    /// Every chat command: the name (with the slash) and what it does. This
    /// table is the single source of truth for the completion palette, the
    /// `/help` notice, and nothing else needs to list them.
    #[test]
    fn login_buffers_typing_and_masks_it() {
        let mut login = LoginScreen::new();
        // The add flow down to the credential line: add row, google's
        // row, the suggested name.
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Enter));
        for c in "sk-abc".chars() {
            login.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(login.input, "sk-abc");
        login.handle_key(key(KeyCode::Backspace));
        assert_eq!(login.input, "sk-ab");
        assert_eq!(
            login.masked_input(),
            "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}"
        );
        assert!(!login.masked_input().contains("sk"));
    }

    #[test]
    fn login_enter_with_a_blank_credential_does_nothing() {
        let mut login = LoginScreen::new();
        login.stage = LoginStage::EnterCredential;
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::None);
        assert_eq!(login.status, LoginStatus::Idle);
        login.input = "   ".into();
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::None);
    }

    #[test]
    fn login_enter_validates_and_locks_until_the_outcome() {
        let mut login = LoginScreen::new();
        login.stage = LoginStage::EnterCredential;
        login.input = " key ".into();
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::Validate);
        assert_eq!(login.status, LoginStatus::Validating);
        // A validation in flight swallows Enter.
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::None);
        assert_eq!(login.status, LoginStatus::Validating);
    }

    #[test]
    fn the_copilot_row_starts_the_flow_instead_of_a_credential_input() {
        let mut login = LoginScreen::new();
        // The add flow down to Copilot's row, then the name stage: enter
        // starts the flow, not a credential input.
        login.handle_key(key(KeyCode::Enter));
        login.selected = PROVIDERS.len() - 1;
        assert_eq!(login.selected_provider(), LoginProvider::Copilot);
        login.handle_key(key(KeyCode::Enter));
        assert_eq!(login.stage, LoginStage::NameInstance);
        assert_eq!(
            login.handle_key(key(KeyCode::Enter)),
            LoginAction::StartFlow
        );
        assert_eq!(login.stage, LoginStage::Flow);
        assert_eq!(login.status, LoginStatus::Validating);
        // A running flow swallows every key but esc.
        assert_eq!(login.handle_key(key(KeyCode::Char('x'))), LoginAction::None);
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::None);
        // Esc cancels back to the instance list, stage state cleared.
        assert_eq!(login.handle_key(key(KeyCode::Esc)), LoginAction::CancelFlow);
        assert_eq!(login.stage, LoginStage::Instances);
        assert_eq!(login.status, LoginStatus::Idle);
        assert_eq!(login.flow_instruction, None);
        assert_eq!(login.flow_waiting, None);
    }

    #[test]
    fn login_error_renders_inline_and_allows_retry() {
        let mut login = LoginScreen::new();
        login.stage = LoginStage::EnterCredential;
        login.input = "bad-key".into();
        login.status = LoginStatus::Validating;
        login.on_validated(&Err("gemini api error (HTTP 403): denied".into()));
        assert_eq!(
            login.status,
            LoginStatus::Error("gemini api error (HTTP 403): denied".into())
        );
        // Error state accepts Enter again (retry).
        assert_eq!(login.handle_key(key(KeyCode::Enter)), LoginAction::Validate);
    }

    #[test]
    fn tab_keys_cycle_the_profile_tabs_and_carry_the_stored_section() {
        let mut login = LoginScreen::new().with_registry(
            &BTreeMap::from([
                (
                    "personal".to_owned(),
                    rhema_config::Profile {
                        google_api_key: Some("p-key".into()),
                        ..rhema_config::Profile::default()
                    },
                ),
                (
                    "work".to_owned(),
                    rhema_config::Profile {
                        openrouter_api_key: Some("w-key".into()),
                        ..rhema_config::Profile::default()
                    },
                ),
            ]),
            "personal",
        );
        assert_eq!(login.profile, "personal");
        assert_eq!(login.stored.google_api_key.as_deref(), Some("p-key"));

        login.handle_key(key(KeyCode::Tab));
        assert_eq!(login.profile, "work");
        assert_eq!(login.stored.openrouter_api_key.as_deref(), Some("w-key"));

        login.handle_key(key(KeyCode::Left));
        assert_eq!(login.profile, "personal");
    }

    #[test]
    fn login_selection_wraps_across_the_providers() {
        let mut login = LoginScreen::new();
        // The provider list lives behind the add row.
        login.handle_key(key(KeyCode::Enter));
        for _ in 0..PROVIDERS.len() {
            login.handle_key(key(KeyCode::Down));
        }
        assert_eq!(login.selected, 0, "down wraps at the end");
        login.handle_key(key(KeyCode::Up));
        assert_eq!(login.selected, PROVIDERS.len() - 1, "up wraps at the start");
    }

    #[test]
    fn the_new_providers_take_masked_api_keys_and_the_stored_row() {
        // The add flow: google's row, down three to Anthropic, the
        // suggested name, then the key.
        let mut login = LoginScreen::new();
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Down));
        assert_eq!(login.selected_provider(), LoginProvider::Anthropic);
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Enter));
        assert_eq!(login.input_label(), "API key: ");
        for c in "sk-ant-01".chars() {
            login.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(login.input, "sk-ant-01");
        assert_eq!(login.masked_input(), "\u{2022}".repeat(9));
        assert!(
            !login.masked_input().contains("sk"),
            "no key material is rendered"
        );
    }

    #[test]
    fn the_name_suggestion_skips_taken_names() {
        let mut login = LoginScreen::new();
        for name in ["openai", "openai-2"] {
            login.stored.instances.insert(
                name.to_owned(),
                rhema_config::ProviderInstance {
                    provider: "openai".into(),
                    api_key: Some("k".into()),
                    base_url: None,
                },
            );
        }
        // Down past the two stored instances to the add row, then into
        // the provider list.
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Enter));
        for _ in 0..4 {
            login.handle_key(key(KeyCode::Down));
        }
        login.handle_key(key(KeyCode::Enter));
        assert_eq!(login.input, "openai-3");
        // Renaming is free: clear and type.
        for _ in 0..8 {
            login.handle_key(key(KeyCode::Backspace));
        }
        for c in "proxy".chars() {
            login.handle_key(key(KeyCode::Char(c)));
        }
        login.handle_key(key(KeyCode::Enter));
        assert_eq!(
            login.pending.as_ref().map(|p| p.name.as_str()),
            Some("proxy")
        );
        assert_eq!(login.stage, LoginStage::EnterCredential);
        // Esc walks back to the name with it intact.
        login.handle_key(key(KeyCode::Esc));
        assert_eq!(login.stage, LoginStage::NameInstance);
        assert_eq!(login.input, "proxy");
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
        PickerScreen::new(
            catalog(
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
            ),
            PickerPurpose::Initial,
        )
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
            PickerPurpose::Initial,
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
            PickerPurpose::Initial,
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
    fn esc_clears_a_non_empty_filter_before_quitting() {
        let mut picker = picker();
        picker.handle_key(key(KeyCode::Char('t')));
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::None);
        assert_eq!(picker.filter, "");
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::Quit);
    }

    #[test]
    fn picker_enter_chooses_nothing_on_an_empty_list() {
        let mut picker = picker();
        picker.filter = "zzz".into();
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
    }

    #[test]
    fn picker_esc_quits_initially_and_returns_to_chat_mid_session() {
        let mut picker = picker();
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::Quit);
        let mut picker = PickerScreen::new(Vec::new(), PickerPurpose::FromChat);
        assert_eq!(
            picker.handle_key(key(KeyCode::Esc)),
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
        }
        chat.handle_key(key(KeyCode::Left), &[], &[]);
        chat.handle_key(key(KeyCode::Left), &[], &[]);
        // cursor sits between "hel" and "o"
        chat.handle_key(key(KeyCode::Char('l')), &[], &[]);
        assert_eq!(chat.input, "hello");
        assert_eq!(chat.cursor, 3);
        chat.handle_key(key(KeyCode::Backspace), &[], &[]);
        assert_eq!(chat.input, "helo");
        chat.handle_key(key(KeyCode::End), &[], &[]);
        chat.handle_key(key(KeyCode::Char('!')), &[], &[]);
        assert_eq!(chat.input, "helo!");
        chat.handle_key(key(KeyCode::Home), &[], &[]);
        chat.handle_key(key(KeyCode::Delete), &[], &[]);
        assert_eq!(chat.input, "elo!");
    }

    #[test]
    fn chat_enter_submits_the_input_text() {
        let mut chat = ChatState::new();
        for c in "fix it".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
        }
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter), &[], &[]),
            ChatAction::Submit("fix it".into())
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
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &[], &[]);
        assert_eq!(chat.input, "x\n");
        chat.handle_key(ctrl('j'), &[], &[]);
        assert_eq!(chat.input, "x\n\n");
        assert_eq!(chat.cursor, 3);
        // Enter itself still submits the whole multi-line text.
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter), &[], &[]),
            ChatAction::Submit("x\n\n".into())
        );
    }

    #[test]
    fn login_paste_lands_on_the_credential_line_without_control_characters() {
        let mut login = LoginScreen::new();
        // The list stages have no text line to fill.
        login.paste("sk-123");
        assert_eq!(login.input, "");

        // The add flow down to the credential line.
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Enter));
        login.handle_key(key(KeyCode::Enter));
        login.paste("sk-12\n34\r\n");
        assert_eq!(login.input, "sk-1234", "a key is a single line");
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
        );
        assert_eq!(chat.input, "@");
        // Plain Ctrl combos stay inert.
        chat.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &[],
            &[],
        );
        assert_eq!(chat.input, "@");
    }

    #[test]
    fn a_multi_line_input_moves_the_cursor_between_lines() {
        let mut chat = ChatState::new();
        chat.insert_str("alpha\nbeta\ngamma");
        assert_eq!(chat.cursor_line_index(), 2, "the end is the last line");
        chat.handle_key(key(KeyCode::Up), &[], &[]);
        assert_eq!(chat.cursor_line_index(), 1);
        assert_eq!(chat.cursor_column(), 4, "column kept");
        chat.handle_key(key(KeyCode::Up), &[], &[]);
        assert_eq!(chat.cursor_line_index(), 0);
        assert_eq!(chat.cursor_column(), 4, "clamped to the shorter line");
        chat.handle_key(key(KeyCode::Up), &[], &[]);
        assert_eq!(chat.cursor_line_index(), 0, "the top stays");
        chat.handle_key(key(KeyCode::Down), &[], &[]);
        chat.handle_key(key(KeyCode::Down), &[], &[]);
        assert_eq!(chat.cursor_line_index(), 2);
    }

    #[test]
    fn home_and_end_stay_within_the_cursor_line() {
        let mut chat = ChatState::new();
        chat.insert_str("alpha\nbeta");
        chat.handle_key(key(KeyCode::Home), &[], &[]);
        assert_eq!(chat.cursor_line_index(), 1);
        assert_eq!(chat.cursor_column(), 0);
        chat.handle_key(key(KeyCode::End), &[], &[]);
        assert_eq!(chat.cursor_column(), 4);
    }

    #[test]
    fn a_long_paste_anchors_at_the_cursor_and_expands_there() {
        let mut chat = ChatState::new();
        let long = "l1\nl2\nl3\nl4";
        chat.insert_str("look at ");
        chat.paste(long);
        assert_eq!(chat.attachments, vec![long.to_owned()]);
        assert_eq!(
            chat.input,
            format!("look at {}", ChatState::MARK),
            "the box shows a chip anchored at the paste, not the text"
        );

        // Enter expands the attachment at its anchor — where the paste
        // was made; clearing happens app-side when the submit is
        // accepted.
        chat.insert_str(" closely");
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter), &[], &[]),
            ChatAction::Submit(format!("look at {long} closely"))
        );
        assert_eq!(
            chat.input,
            format!("look at {} closely", ChatState::MARK),
            "declined keeps the text"
        );
        assert_eq!(chat.attachments.len(), 1);
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
        chat.handle_key(key(KeyCode::Backspace), &[], &[]);
        assert_eq!(chat.attachments, vec![first.to_owned()]);
        assert_eq!(chat.input, ChatState::MARK.to_string());

        // Deleting the remaining anchor from before it works the same.
        chat.handle_key(key(KeyCode::Home), &[], &[]);
        chat.handle_key(key(KeyCode::Delete), &[], &[]);
        assert!(chat.attachments.is_empty());
        assert_eq!(chat.input, "");
    }

    #[test]
    fn a_short_paste_joins_the_input_inline() {
        let mut chat = ChatState::new();
        chat.paste("l1\nl2\nl3");
        assert!(chat.attachments.is_empty());
        assert_eq!(chat.input, "l1\nl2\nl3");
    }

    #[test]
    fn shift_enter_adds_a_line() {
        let mut chat = ChatState::new();
        chat.insert_str("x");
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &[], &[]);
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
        );
        assert_eq!(chat.input, "A");
        // Ghostty & co. send the shifted char with SHIFT cleared
        // (crossterm's alternate-key path): untouched.
        chat.handle_key(key(KeyCode::Char('B')), &[], &[]);
        assert_eq!(chat.input, "AB");
        // Plain lowercase typing stays lowercase.
        chat.handle_key(key(KeyCode::Char('c')), &[], &[]);
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
            chat.handle_key(key(KeyCode::Enter), &[], &[]),
            ChatAction::Submit(String::new())
        );
    }

    #[test]
    fn chat_scroll_moves_with_page_keys_and_never_goes_negative() {
        let mut chat = ChatState::new();
        chat.handle_key(key(KeyCode::PageUp), &[], &[]);
        chat.handle_key(key(KeyCode::PageUp), &[], &[]);
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP);
        chat.scroll_by(-3);
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP - 3);
        chat.handle_key(key(KeyCode::PageDown), &[], &[]);
        chat.handle_key(key(KeyCode::PageDown), &[], &[]);
        chat.handle_key(key(KeyCode::PageDown), &[], &[]);
        assert_eq!(chat.scroll, 0);
        chat.scroll_by(5);
        chat.handle_key(key(KeyCode::Enter), &[], &[]);
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
        }
        assert_eq!(
            crate::screens::filtered_commands("/").len(),
            crate::commands::COMMANDS.len()
        );
        for c in "cl".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
        }
        chat.handle_key(key(KeyCode::Down), &[], &[]);
        chat.handle_key(key(KeyCode::Down), &[], &[]);
        assert_eq!(chat.palette_index, 2);
        chat.handle_key(key(KeyCode::Up), &[], &[]);
        assert_eq!(chat.palette_index, 1);
        chat.handle_key(key(KeyCode::Tab), &[], &[]);
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
            chat.handle_key(key(KeyCode::Char(c)), &models, &[]);
        }
        // The empty argument lists the whole catalog; the provider filter
        // narrows like the picker's.
        let ids: Vec<String> = crate::screens::palette_rows(&chat.input, &models, &[])
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Model { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["gemini-3.8-pro", "openrouter/auto"]);
        for c in "openrouter".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &models, &[]);
        }
        let ids: Vec<String> = crate::screens::palette_rows(&chat.input, &models, &[])
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Model { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["openrouter/auto"]);
        // The vendor slash keeps narrowing instead of breaking the match.
        chat.handle_key(key(KeyCode::Char('/')), &models, &[]);
        let ids: Vec<String> = crate::screens::palette_rows(&chat.input, &models, &[])
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Model { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["openrouter/auto"], "openrouter/ stays alive");
        // Tab completes the argument, keeping the command prefix.
        chat.handle_key(key(KeyCode::Tab), &models, &[]);
        assert_eq!(chat.input, "/model openrouter/auto");
        assert_eq!(chat.cursor, "/model openrouter/auto".chars().count());
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &profiles);
        }
        // The name narrows; so does the provider text a row carries.
        let names = |input: &str| {
            crate::screens::palette_rows(input, &[], &profiles)
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
        chat.handle_key(key(KeyCode::Tab), &[], &profiles);
        assert_eq!(chat.input, "/profile work");
    }

    #[test]
    fn editing_the_input_resets_the_palette_selection() {
        let mut chat = ChatState::new();
        for c in "/".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[]);
        }
        chat.handle_key(key(KeyCode::Down), &[], &[]);
        chat.handle_key(key(KeyCode::Backspace), &[], &[]);
        assert_eq!(chat.palette_index, 0);
    }

    #[test]
    fn chat_ctrl_combos_are_left_to_the_app() {
        let mut chat = ChatState::new();
        assert_eq!(
            chat.handle_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &[],
                &[]
            ),
            ChatAction::None
        );
        assert_eq!(chat.input, "");
    }
}
