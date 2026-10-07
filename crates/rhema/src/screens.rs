//! The three screens' input handling and outcome logic — pure state, no
//! terminal. The renderer reads these structs; [`crate::app`] routes their
//! actions into transitions and effects.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_provider_google::GeminiModel;

/// Which backend a login provider row configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProvider {
    /// Google Gemini, by API key.
    Google,
    /// A llama.cpp server, by URL.
    LlamaCpp,
    /// OpenRouter, by API key.
    OpenRouter,
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
    /// The provider list: arrows to choose, enter to continue.
    SelectProvider,
    /// The credential input for the chosen provider.
    EnterCredential,
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

/// What one login keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAction {
    /// Nothing to do.
    None,
    /// Validate the entered key against the provider.
    Validate,
    /// Leave the app (no key to log in with, nothing else to show).
    Quit,
}

/// The login screen: profile tabs, provider list, masked API-key input,
/// validation status.
#[derive(Debug)]
pub struct LoginScreen {
    /// Which half of the two-stage login is showing.
    pub stage: LoginStage,
    /// Index into [`PROVIDERS`] — the highlighted provider row.
    pub selected: usize,
    /// The API key or server URL being typed (credential stage).
    pub input: String,
    /// Current status.
    pub status: LoginStatus,
    /// What Esc does here.
    pub purpose: LoginPurpose,
    /// The registry of profiles; the tabs cycle through its keys.
    pub profiles: BTreeMap<String, rhema_config::Profile>,
    /// The profile this login writes into — the selected tab.
    pub profile: String,
    /// The stored settings of that profile: what each provider row
    /// reports as registered.
    pub stored: rhema_config::Profile,
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginScreen {
    /// A fresh login screen: Google selected, empty key, idle, Esc quits.
    pub fn new() -> Self {
        Self {
            input: String::new(),
            selected: 0,
            stage: LoginStage::SelectProvider,
            status: LoginStatus::Idle,
            purpose: LoginPurpose::Initial,
            profiles: BTreeMap::new(),
            profile: "personal".to_owned(),
            stored: rhema_config::Profile::default(),
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

    /// Preselects the provider row matching the stored settings' provider
    /// string, so a profile's login opens on its own row. Unmatched
    /// spellings keep Google selected.
    pub fn select_stored_provider(self, provider: Option<&str>) -> Self {
        let selected = PROVIDERS
            .iter()
            .position(|entry| match entry.kind {
                LoginProvider::Google => provider == Some("google"),
                LoginProvider::LlamaCpp => {
                    provider == Some("llama-cpp")
                        || provider == Some("llama_cpp")
                        || provider == Some("llamacpp")
                }
                LoginProvider::OpenRouter => provider == Some("openrouter"),
            })
            .unwrap_or(0);
        Self { selected, ..self }
    }

    /// A login screen opened from the chat: Esc returns there.
    pub fn for_chat() -> Self {
        Self {
            purpose: LoginPurpose::FromChat,
            ..Self::new()
        }
    }

    /// The hint under the input, per the highlighted provider and what Esc
    /// does.
    pub fn input_hint(&self) -> String {
        let base = match self.stage {
            LoginStage::SelectProvider => "arrows choose · enter to log in".to_owned(),
            LoginStage::EnterCredential => {
                let what = match self.selected_provider() {
                    LoginProvider::LlamaCpp => "the server URL",
                    _ => "the API key",
                };
                format!("enter {what}, then press enter")
            }
        };
        match self.purpose {
            LoginPurpose::FromChat => format!("{base} · esc back"),
            LoginPurpose::Initial => base,
        }
    }

    /// The backend the highlighted row configures.
    pub fn selected_provider(&self) -> LoginProvider {
        PROVIDERS[self.selected.min(PROVIDERS.len() - 1)].kind
    }

    /// The input's label, per the highlighted provider.
    pub fn input_label(&self) -> &'static str {
        match self.selected_provider() {
            LoginProvider::LlamaCpp => "server URL: ",
            LoginProvider::Google | LoginProvider::OpenRouter => "API key: ",
        }
    }

    /// The rendered echo of the input: API keys mask to bullets (no key
    /// material ever rendered); a server URL is not secret, so it shows.
    pub fn masked_input(&self) -> String {
        match self.selected_provider() {
            LoginProvider::LlamaCpp => self.input.clone(),
            LoginProvider::Google | LoginProvider::OpenRouter => {
                "\u{2022}".repeat(self.input.chars().count())
            }
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> LoginAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return LoginAction::None;
        }
        match self.stage {
            LoginStage::SelectProvider => self.handle_provider_key(key),
            LoginStage::EnterCredential => self.handle_credential_key(key),
        }
    }

    /// Keys on the provider list: arrows choose, enter descends into the
    /// credential input, esc leaves the login.
    fn handle_provider_key(&mut self, key: KeyEvent) -> LoginAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_selection(key.code == KeyCode::Down);
                LoginAction::None
            }
            KeyCode::Enter => {
                self.input.clear();
                self.status = LoginStatus::Idle;
                self.stage = LoginStage::EnterCredential;
                LoginAction::None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.cycle_profile_tab(key.code == KeyCode::Right || key.code == KeyCode::Tab);
                LoginAction::None
            }
            KeyCode::Esc => LoginAction::Quit,
            _ => LoginAction::None,
        }
    }

    /// Keys while typing the credential: enter validates, esc returns to
    /// the provider list.
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
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                self.input.clear();
                self.status = LoginStatus::Idle;
                self.stage = LoginStage::SelectProvider;
                LoginAction::None
            }
            _ => LoginAction::None,
        }
    }

    /// Moves the selection around the provider list, wrapping.
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
    }

    /// Moves the selection among the providers, wrapping.
    fn move_selection(&mut self, down: bool) {
        let count = PROVIDERS.len() as isize;
        let step = if down { 1 } else { -1 };
        self.selected = ((self.selected as isize + step).rem_euclid(count)) as usize;
    }

    /// Enter: validate the key, unless a validation is already in flight
    /// or the input is blank.
    fn submit(&mut self) -> LoginAction {
        if matches!(self.status, LoginStatus::Validating) {
            return LoginAction::None;
        }
        if self.input.trim().is_empty() {
            return LoginAction::None;
        }
        self.status = LoginStatus::Validating;
        LoginAction::Validate
    }

    /// Records the validation outcome: an error renders inline and allows
    /// retrying; success leaves the transition to the app (it needs the
    /// models and the store).
    pub fn on_validated(&mut self, result: &Result<Vec<GeminiModel>, String>) {
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
    /// The highlighted model was chosen.
    Choose(String),
    /// Leave the app.
    Quit,
    /// Return to the chat.
    BackToChat,
    /// Retry the failed model fetch.
    Retry,
}

/// The model picker: text chat models sorted by id, a type-to-filter
/// prompt, and one highlighted row.
#[derive(Debug)]
pub struct PickerScreen {
    /// The models to pick from (text chat only), sorted by id.
    pub models: Vec<GeminiModel>,
    /// The provider the models come from; the row prefix.
    pub provider: &'static str,
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
    /// A picker over `models` (sorted by id, text chat models only),
    /// initially unfiltered.
    pub fn new(
        mut models: Vec<GeminiModel>,
        purpose: PickerPurpose,
        provider: &'static str,
    ) -> Self {
        models.retain(Self::usable);
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            models,
            provider,
            filter: String::new(),
            selected: 0,
            status: PickerStatus::Ready,
            purpose,
        }
    }

    /// The models this coding agent can drive: text chat models.
    fn usable(model: &GeminiModel) -> bool {
        model.supports_chat() && model.is_text()
    }

    /// Replaces the model list (scoped and re-sorted) and clamps the
    /// selection.
    pub fn set_models(&mut self, models: Vec<GeminiModel>) {
        let mut models = models;
        models.retain(Self::usable);
        self.models = models;
        self.models.sort_by(|a, b| a.id.cmp(&b.id));
        self.clamp_selection();
    }

    /// The models matching the filter: case-insensitive substring on id or
    /// display name, in the sorted order.
    pub fn filtered(&self) -> Vec<&GeminiModel> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|model| {
                needle.is_empty()
                    || model.id.to_lowercase().contains(&needle)
                    || model.display_name.to_lowercase().contains(&needle)
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
            Some(model) => PickerAction::Choose(model.id.clone()),
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
    /// The input being typed.
    pub input: String,
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

    /// Byte offset of the cursor inside `input` (a char boundary).
    fn byte_offset(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// Handles one keypress. Ctrl combos are the app's (quit, model
    /// picker); Esc is the app's (approval decline / quit confirm).
    pub fn handle_key(&mut self, key: KeyEvent) -> ChatAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ChatAction::None;
        }
        match key.code {
            KeyCode::Char(c) => {
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
                    self.input.replace_range(start..end, "");
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
                    self.input.replace_range(start..end, "");
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
                self.cursor = 0;
                ChatAction::None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                ChatAction::None
            }
            // The command palette owns the vertical arrows while a slash
            // input filters it.
            KeyCode::Up | KeyCode::Down
                if self.input.starts_with('/') && !filtered_commands(&self.input).is_empty() =>
            {
                let count = filtered_commands(&self.input).len();
                if key.code == KeyCode::Down {
                    self.palette_index = (self.palette_index + 1).min(count - 1);
                } else {
                    self.palette_index = self.palette_index.saturating_sub(1);
                }
                ChatAction::None
            }
            KeyCode::Tab if self.input.starts_with('/') => {
                let candidates = filtered_commands(&self.input);
                if let Some(spec) =
                    candidates.get(self.palette_index.min(candidates.len().saturating_sub(1)))
                {
                    self.input = spec.name.to_owned();
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
                // A new prompt goes to the live tail; stop pinning history.
                self.scroll = 0;
                ChatAction::Submit(self.input.clone())
            }
            _ => ChatAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_provider_google::GeminiModel;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn model(id: &str, display: &str, chat: bool) -> GeminiModel {
        assert!(chat, "pickers only list chat models");
        GeminiModel {
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
        // Pick a provider first: enter descends into the credential stage.
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
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Down));
        login.handle_key(key(KeyCode::Down));
        assert_eq!(login.selected, 0, "down wraps at the end");
        login.handle_key(key(KeyCode::Up));
        assert_eq!(login.selected, PROVIDERS.len() - 1, "up wraps at the start");
    }

    // --- Picker ---

    fn picker() -> PickerScreen {
        PickerScreen::new(
            vec![
                model("gemini-3.8-pro", "Gemini 3.8 Pro", true),
                model("gemini-3.8-flash", "Gemini 3.8 Flash", true),
                // Non-text families are dropped by the picker itself.
                model("gemini-3.8-flash-tts", "Gemini 3.8 Flash TTS", true),
                model("veo-3.1-generate-preview", "Veo 3.1", true),
            ],
            PickerPurpose::Initial,
            "google",
        )
    }

    #[test]
    fn picker_sorts_by_id() {
        let picker = picker();
        let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemini-3.8-flash", "gemini-3.8-pro"]);
    }

    #[test]
    fn non_text_models_are_dropped_from_the_list() {
        let picker = picker();
        let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
        assert!(!ids.contains(&"gemini-3.8-flash-tts"));
        assert!(!ids.contains(&"veo-3.1-generate-preview"));
        assert_eq!(picker.provider, "google");
    }

    #[test]
    fn picker_filter_matches_id_or_display_name_case_insensitively() {
        let mut picker = picker();
        picker.filter = "FLASH".into();
        let ids: Vec<&str> = picker.filtered().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemini-3.8-flash"]);

        picker.filter = "3.8 pro".into();
        let ids: Vec<&str> = picker.filtered().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemini-3.8-pro"]);

        picker.filter = "zzz".into();
        assert!(picker.filtered().is_empty());
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
        let ids: Vec<&str> = picker.filtered().iter().map(|m| m.id.as_str()).collect();
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
            PickerAction::Choose("gemini-3.8-pro".into())
        );
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
        let mut picker = PickerScreen::new(Vec::new(), PickerPurpose::FromChat, "google");
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
        picker.set_models(vec![model("a-model", "A", true)]);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.models[0].id, "a-model");
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
            chat.handle_key(key(KeyCode::Char(c)));
        }
        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(key(KeyCode::Left));
        // cursor sits between "hel" and "o"
        chat.handle_key(key(KeyCode::Char('l')));
        assert_eq!(chat.input, "hello");
        assert_eq!(chat.cursor, 3);
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "helo");
        chat.handle_key(key(KeyCode::End));
        chat.handle_key(key(KeyCode::Char('!')));
        assert_eq!(chat.input, "helo!");
        chat.handle_key(key(KeyCode::Home));
        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(chat.input, "elo!");
    }

    #[test]
    fn chat_enter_submits_the_input_text() {
        let mut chat = ChatState::new();
        for c in "fix it".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Submit("fix it".into())
        );
        // The input box is NOT cleared here: the app clears it only when
        // it accepts the submit (a rejected submit — empty or mid-turn —
        // must not lose typed text).
        assert_eq!(chat.input, "fix it");
    }

    #[test]
    fn chat_enter_on_an_empty_input_yields_an_empty_submit() {
        let mut chat = ChatState::new();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Submit(String::new())
        );
    }

    #[test]
    fn chat_scroll_moves_with_page_keys_and_never_goes_negative() {
        let mut chat = ChatState::new();
        chat.handle_key(key(KeyCode::PageUp));
        chat.handle_key(key(KeyCode::PageUp));
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP);
        chat.scroll_by(-3);
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP - 3);
        chat.handle_key(key(KeyCode::PageDown));
        chat.handle_key(key(KeyCode::PageDown));
        chat.handle_key(key(KeyCode::PageDown));
        assert_eq!(chat.scroll, 0);
        chat.scroll_by(5);
        chat.handle_key(key(KeyCode::Enter));
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
            chat.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            crate::screens::filtered_commands("/").len(),
            crate::commands::COMMANDS.len()
        );
        for c in "cl".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
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
            chat.handle_key(key(KeyCode::Char(c)));
        }
        chat.handle_key(key(KeyCode::Down));
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(chat.palette_index, 2);
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.palette_index, 1);
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(chat.input, "/model");
        assert_eq!(chat.cursor, "/model".chars().count());
    }

    #[test]
    fn editing_the_input_resets_the_palette_selection() {
        let mut chat = ChatState::new();
        for c in "/".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
        chat.handle_key(key(KeyCode::Down));
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.palette_index, 0);
    }

    #[test]
    fn chat_ctrl_combos_are_left_to_the_app() {
        let mut chat = ChatState::new();
        assert_eq!(
            chat.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            ChatAction::None
        );
        assert_eq!(chat.input, "");
    }
}
