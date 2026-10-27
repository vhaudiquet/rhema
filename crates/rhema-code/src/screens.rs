//! The three screens' input handling and outcome logic — pure state, no
//! terminal. The one exception is the chat palette's path rows, which
//! list the filesystem. The renderer reads these structs;
//! [`crate::app`] routes their actions into transitions and effects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

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
    /// Z.ai's GLM Coding Plan, by API key (their Anthropic-compatible
    /// endpoint).
    Zai,
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
        name: "Z.ai (GLM Coding Plan)",
        kind: LoginProvider::Zai,
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

/// The config's profiles as rows, sorted by name: the profile picker's
/// list and the settings Profiles page's snapshot.
pub fn profile_rows(
    profiles: &BTreeMap<String, rhema_config::Profile>,
    active: Option<&str>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> Vec<ProfileRow> {
    let mut entries: Vec<ProfileRow> = profiles
        .iter()
        .map(|(name, profile)| ProfileRow {
            name: name.clone(),
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            color: profile.color.clone(),
            usable: profile.has_credentials(secrets),
            active: Some(name.as_str()) == active,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
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
    pub fn new(
        profiles: &BTreeMap<String, rhema_config::Profile>,
        active: Option<&str>,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Self {
        Self {
            entries: profile_rows(profiles, active, secrets),
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

// --- Sandbox ---

/// One row of the settings Sandbox page: one kind of the v1 set, its
/// stored fact, and the marks. One instance per kind — the profile's
/// `sandbox` section holds a single set of fields, so adding an
/// instance of a kind means filling that kind's field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRow<'a> {
    /// The instance's name — `local` for the host's own row, empty on
    /// the add row.
    pub name: &'a str,
    /// The kind the instance runs: `local` on the host's own row.
    pub kind: crate::args::SandboxChoice,
    /// What the kind execs in — the container, the LXD instance, the
    /// host, the url — or the hint naming what the add flow asks for.
    pub detail: String,
    /// Whether this machine offered the kind at the startup probe
    /// (`local` always).
    pub offered: bool,
    /// Whether this workspace last ran `bash` under this kind.
    pub remembered: bool,
    /// Whether this session's `bash` currently runs under this kind.
    pub active: bool,
    /// Whether this is the add-a-new-instance row.
    pub add: bool,
}

/// What one add-sandbox-dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDialogAction {
    /// Nothing to do.
    None,
    /// Enter accepted the kind's value: the app stores the instance.
    Submit,
    /// Esc left the dialog (from the kind list).
    Close,
}

/// Which half of the add-sandbox dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDialogStage {
    /// The kind list: docker, ssh, http — `local` needs nothing added.
    SelectKind,
    /// The value input: what the chosen kind names its instance by.
    EnterValue,
}

/// The kinds the Sandbox page lists, in the spec's canonical order.
/// The ready set only: `local` and `docker`. ssh and http are not
/// ready features — ssh has no carry story for the working tree yet,
/// http presumes a `sandboxd` to reach — so they stay out of sight
/// here and remain config-file citizens: store their section in
/// `config.yaml` and `/sandbox <kind>` runs them when offered.
/// The kinds the add dialog offers: the page's kinds minus `local` —
/// the local instance exists by doing nothing.
pub const SANDBOX_ADD_KINDS: &[crate::args::SandboxChoice] = &[
    crate::args::SandboxChoice::Docker,
    crate::args::SandboxChoice::Lxd,
];

/// The pop-in dialog over the Sandbox page: pick the kind, then type
/// what it names its instance by — the container, the `user@host`, or
/// the daemon URL. Storing is the app's; the dialog collects.
#[derive(Debug)]
pub struct AddSandboxDialog {
    /// Which half of the dialog is showing.
    pub stage: SandboxDialogStage,
    /// The highlighted row of the kind list.
    pub selected: usize,
    /// The kind being added, from the pick on.
    pub kind: crate::args::SandboxChoice,
    /// The line being typed on the value stage.
    pub input: String,
}

impl AddSandboxDialog {
    /// A dialog at the kind list.
    pub fn new() -> Self {
        Self {
            stage: SandboxDialogStage::SelectKind,
            selected: 0,
            kind: SANDBOX_ADD_KINDS[0],
            input: String::new(),
        }
    }

    /// The kind the highlighted row stands for.
    pub fn selected_kind(&self) -> crate::args::SandboxChoice {
        SANDBOX_ADD_KINDS[self.selected.min(SANDBOX_ADD_KINDS.len() - 1)]
    }

    /// Enter on the kind list: descend into the value input.
    fn choose_kind(&mut self) {
        self.kind = self.selected_kind();
        self.input = String::new();
        self.stage = SandboxDialogStage::EnterValue;
    }

    /// The value stage's prompt.
    pub fn input_label(&self) -> &'static str {
        match self.kind {
            crate::args::SandboxChoice::Docker => "container: ",
            crate::args::SandboxChoice::Lxd => "instance: ",
            crate::args::SandboxChoice::Ssh => "host: ",
            crate::args::SandboxChoice::Http => "url: ",
            crate::args::SandboxChoice::Local => "",
        }
    }

    /// The hint under the input, per the stage.
    pub fn input_hint(&self) -> String {
        match self.stage {
            SandboxDialogStage::SelectKind => {
                "arrows choose · enter to continue · esc cancels".to_owned()
            }
            SandboxDialogStage::EnterValue => format!(
                "{}· ctrl-v paste · enter stores · esc back",
                self.input_label()
            ),
        }
    }

    /// The accepted kind and value; the app calls this on the Submit
    /// action. Blank values never reach here (handle_key holds them).
    pub fn submitted(&self) -> (crate::args::SandboxChoice, String) {
        (self.kind, self.input.trim().to_owned())
    }

    /// Bracketed paste onto the value line.
    pub fn paste(&mut self, text: &str) {
        if self.stage == SandboxDialogStage::EnterValue {
            self.input.push_str(text);
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> SandboxDialogAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return SandboxDialogAction::None;
        }
        match self.stage {
            SandboxDialogStage::SelectKind => match key.code {
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(SANDBOX_ADD_KINDS.len() - 1);
                    SandboxDialogAction::None
                }
                KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    SandboxDialogAction::None
                }
                KeyCode::Enter => {
                    self.choose_kind();
                    SandboxDialogAction::None
                }
                KeyCode::Esc => SandboxDialogAction::Close,
                _ => SandboxDialogAction::None,
            },
            SandboxDialogStage::EnterValue => match key.code {
                KeyCode::Char(c) => {
                    self.input.push(c);
                    SandboxDialogAction::None
                }
                KeyCode::Backspace => {
                    self.input.pop();
                    SandboxDialogAction::None
                }
                KeyCode::Enter => {
                    if self.input.trim().is_empty() {
                        SandboxDialogAction::None
                    } else {
                        SandboxDialogAction::Submit
                    }
                }
                KeyCode::Esc => {
                    self.stage = SandboxDialogStage::SelectKind;
                    self.input = String::new();
                    SandboxDialogAction::None
                }
                _ => SandboxDialogAction::None,
            },
        }
    }
}

impl Default for AddSandboxDialog {
    fn default() -> Self {
        Self::new()
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

// --- Settings ---

/// A section of the settings screen, in sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    /// The profile's provider, model, and logged-in instances — the
    /// login flow lives here now.
    Providers,
    /// The named profiles: add, remove, switch, and their name colors.
    Profiles,
    /// Where `bash` runs: the sandbox kinds, their instances, the
    /// active one, and the add flow.
    Sandbox,
    /// The theme and the footer's icon flavor.
    Appearance,
    /// Cross-session memory: a status page today.
    Memory,
    /// The discovered skills.
    Skills,
    /// The discovered subagent definitions, editable in `$EDITOR`.
    Subagents,
    /// The allowance, the session's cost, the last turn's tokens.
    Usage,
    /// Version, repository, paths.
    About,
}

impl SettingsSection {
    /// Every section, in sidebar order.
    pub const ALL: &'static [SettingsSection] = &[
        SettingsSection::Providers,
        SettingsSection::Profiles,
        SettingsSection::Sandbox,
        SettingsSection::Appearance,
        SettingsSection::Memory,
        SettingsSection::Skills,
        SettingsSection::Subagents,
        SettingsSection::Usage,
        SettingsSection::About,
    ];

    /// The label the sidebar draws for this section.
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Providers => "Providers",
            SettingsSection::Profiles => "Profiles",
            SettingsSection::Sandbox => "Sandbox",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Memory => "Memory",
            SettingsSection::Skills => "Skills",
            SettingsSection::Subagents => "Subagents",
            SettingsSection::Usage => "Usage",
            SettingsSection::About => "About",
        }
    }
}

/// What one settings keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    /// Nothing to do.
    None,
    /// Esc or `q` (outside the add dialog): back to the chat.
    Back,
    /// Enter on a logged-in instance: it becomes the active one.
    ActivateInstance,
    /// `d` on a logged-in instance: forget it.
    RemoveInstance,
    /// Enter on the add row: the add-instance dialog opens.
    OpenDialog,
    /// Enter on a profile row: the session switches to it.
    SelectProfile,
    /// `d` on a profile row: remove it from the config.
    RemoveProfile,
    /// `c` on a profile row: cycle the name color through the defaults.
    CycleProfileColor,
    /// Enter on the add row: the add-profile dialog opens.
    OpenProfileDialog,
    /// The add-profile dialog accepted a name: create the profile.
    SubmitProfileDialog,
    /// Esc left the add-profile dialog.
    CloseProfileDialog,
    /// The dialog validated a typed key: the fetch is the validation.
    Validate,
    /// The dialog named a flow provider: the app spawns its flow.
    StartFlow,
    /// The dialog closed (esc at any stage); a running flow task dies
    /// with it.
    CloseDialog,
    /// Cycle to the next color palette.
    CyclePalette,
    /// Toggle the theme.
    ToggleTheme,
    /// Toggle the icon flavor.
    ToggleIcons,
    /// Enter or `e` on a subagent row: open the definition in `$EDITOR`.
    /// The built-in has no file — the app copies it into the global
    /// directory first, where it shadows the built-in from then on.
    EditSubagent,
    /// Enter or Space on a skills row: flip the skill's on/off state.
    ToggleSkill,
    /// Enter on a sandbox kind row: run `bash` under it.
    SelectSandbox(crate::args::SandboxChoice),
    /// Enter on the sandbox add row: the add-sandbox dialog opens.
    OpenSandboxDialog,
    /// Enter on a sandbox instance row: run `bash` in it.
    SelectSandboxInstance,
    /// `d` on a sandbox instance row: remove it.
    RemoveSandboxInstance,
    /// The add-sandbox dialog accepted a kind and value: store it.
    SubmitSandboxDialog,
    /// Esc left the add-sandbox dialog.
    CloseSandboxDialog,
}

/// One row of the Providers page's instance list: a stored instance, or
/// the add row.
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

/// One discovered skill row of the Skills page and the onboarding
/// wizard's skills step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    /// The skill's folder name.
    pub name: String,
    /// The frontmatter description the model triggers on.
    pub description: String,
    /// The frontmatter license, when the skill declares one — shown
    /// under the description, dimmer than it.
    pub license: Option<String>,
    /// Where the skill lives.
    pub source: SkillSource,
    /// Whether the skill's trigger reaches the system prompt — the
    /// config's `skills_disabled` set, inverted. Off rows still list,
    /// dimmed; only the prompt filters them out.
    pub enabled: bool,
}

/// Where a skill lives: shipped with rhema, in the config directory, or
/// in the workspace — the same shadowing order discovery reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Seeded by rhema from the binary's bundled set.
    Bundled,
    /// A folder under the config directory.
    Global,
    /// A folder under the workspace's `.rhema/skills`.
    Workspace,
}

impl SkillSource {
    /// The tag the page draws after the name.
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::Bundled => "bundled",
            SkillSource::Global => "global",
            SkillSource::Workspace => "workspace",
        }
    }
}

/// Where a subagent definition lives. The same discovery rules as the
/// skills': the global directory, the workspace, project shadowing
/// global — and one definition that is not on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSource {
    /// The built-in `general-purpose`, present without any file.
    Builtin,
    /// A folder under the config directory.
    Global,
    /// A folder under the workspace's `.rhema/subagents`.
    Workspace,
}

impl SubagentSource {
    /// The tag the page draws after the name.
    pub fn label(self) -> &'static str {
        match self {
            SubagentSource::Builtin => "built-in",
            SubagentSource::Global => "global",
            SubagentSource::Workspace => "workspace",
        }
    }
}

/// One discovered subagent row of the Subagents page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRow {
    /// The definition's folder name.
    pub name: String,
    /// The frontmatter description the parent triggers on.
    pub description: String,
    /// The model the subagent's requests carry; `None` inherits the
    /// parent's.
    pub model: Option<String>,
    /// The step budget; `None` uses the discovery default.
    pub max_steps: Option<u32>,
    /// Where the definition lives.
    pub source: SubagentSource,
    /// The `SUBAGENT.md` to edit; `None` only for the built-in, whose
    /// first edit copies it into the global directory.
    pub path: Option<PathBuf>,
}

/// What one add-dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    /// Nothing to do.
    None,
    /// Validate the entered key against the provider.
    Validate,
    /// The named provider logs in through an interactive flow: spawn it
    /// (the stage already flipped on the dialog).
    StartFlow,
    /// Esc left the dialog from a quiet stage: close it.
    Close,
    /// Esc left the dialog while its flow ran: close it and drop the
    /// flow task.
    CloseCancelFlow,
}

/// Which half of the add-instance dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogStage {
    /// The provider list: arrows to choose, enter to continue.
    SelectProvider,
    /// The name input for the instance being added.
    NameInstance,
    /// The credential input — llama.cpp and OpenAI ask a URL first,
    /// then a key.
    EnterCredential,
    /// An interactive flow (device code and friends) is running: its
    /// instruction and progress render from the dialog's `flow_*`
    /// fields, Esc cancels, and the credential arrives through the
    /// app — the dialog never sees the protocol.
    Flow,
}

/// Where the add-instance dialog stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogStatus {
    /// Waiting for input.
    Idle,
    /// A key was submitted; validation is in flight. Enter is inert
    /// until the outcome arrives.
    Validating,
    /// The last validation failed; the text is the provider's error.
    Error(String),
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

/// The pop-in dialog over the Providers page: the provider list for a
/// new instance, then its name, then its key or server URL — the login
/// flow, mounted on the settings menu.
#[derive(Debug)]
pub struct AddInstanceDialog {
    /// Which half of the dialog is showing.
    pub stage: DialogStage,
    /// The highlighted row on the provider list.
    pub selected: usize,
    /// The line being typed: the instance name on `NameInstance`, the
    /// credential on `EnterCredential`.
    pub input: String,
    /// Current status.
    pub status: DialogStatus,
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
    /// The instance names taken when the dialog opened: the name
    /// suggestion dedupes against them. A snapshot, not a window — the
    /// dialog is modal, nothing else can rename under it.
    instances: BTreeMap<String, rhema_config::ProviderInstance>,
}

impl AddInstanceDialog {
    /// A dialog at the provider list, deduping name suggestions against
    /// the profile's stored instances.
    pub fn new(stored: &rhema_config::Profile) -> Self {
        Self {
            stage: DialogStage::SelectProvider,
            selected: 0,
            input: String::new(),
            status: DialogStatus::Idle,
            pending: None,
            second_input: false,
            flow_instruction: None,
            flow_waiting: None,
            instances: stored.instances.clone(),
        }
    }

    /// The backend the highlighted provider row configures.
    pub fn selected_provider(&self) -> LoginProvider {
        PROVIDERS[self.selected.min(PROVIDERS.len() - 1)].kind
    }

    /// Moves the provider-list selection to `index` (a click); out-of-
    /// range clicks clamp to the list.
    pub fn select_provider_row(&mut self, index: usize) {
        if self.stage == DialogStage::SelectProvider {
            self.selected = index.min(PROVIDERS.len() - 1);
        }
    }

    /// Enter (or a click) on the provider list: descend into the name
    /// input — or the interactive flow, for the providers that log in
    /// that way.
    pub fn choose_provider(&mut self) -> DialogAction {
        let provider = self.selected_provider();
        self.pending = Some(PendingInstance {
            provider,
            name: self.name_suggestion(provider),
            base_url: None,
        });
        self.input = self.pending_name();
        self.status = DialogStatus::Idle;
        self.stage = DialogStage::NameInstance;
        DialogAction::None
    }

    /// The hint under the input, per the stage.
    pub fn input_hint(&self) -> String {
        match self.stage {
            DialogStage::SelectProvider => {
                "arrows choose · enter to log in · esc cancels".to_owned()
            }
            DialogStage::NameInstance => {
                "name this login · enter to continue · esc back".to_owned()
            }
            DialogStage::EnterCredential => {
                let what = match (self.pending.as_ref().map(|p| p.provider), self.second_input) {
                    (Some(LoginProvider::LlamaCpp), false) => "enter the server URL",
                    (Some(LoginProvider::LlamaCpp), true) => {
                        "enter the bearer key (empty for none)"
                    }
                    (Some(LoginProvider::OpenAi), false) => {
                        "enter the base URL (empty for api.openai.com)"
                    }
                    (Some(LoginProvider::Zai), false) => {
                        "enter the base URL (empty for the coding plan route)"
                    }
                    _ => "enter the API key",
                };
                format!("{what} · ctrl-v paste · enter when done · esc back")
            }
            DialogStage::Flow => "esc cancels the login".to_owned(),
        }
    }

    /// The input's label, per the stage's prompt.
    pub fn input_label(&self) -> &'static str {
        match self.stage {
            DialogStage::NameInstance => "name: ",
            DialogStage::EnterCredential => {
                match (self.pending.as_ref().map(|p| p.provider), self.second_input) {
                    (Some(LoginProvider::LlamaCpp), false) => "server URL: ",
                    (Some(LoginProvider::OpenAi), false) => "base URL: ",
                    (Some(LoginProvider::Zai), false) => "base URL: ",
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
            DialogStage::NameInstance => false,
            DialogStage::EnterCredential => match self.pending.as_ref().map(|p| p.provider) {
                Some(LoginProvider::LlamaCpp)
                | Some(LoginProvider::OpenAi)
                | Some(LoginProvider::Zai) => self.second_input,
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
            DialogStage::NameInstance | DialogStage::EnterCredential
        ) {
            self.input.extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> DialogAction {
        // ctrl-v and ctrl-shift-v are paste in terminals that leave them
        // unbound: the key reads the clipboard, carrying the text a
        // bracketed paste would have. Only on the text lines.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                self.stage,
                DialogStage::NameInstance | DialogStage::EnterCredential
            )
        {
            if let Some(text) = crate::clipboard::read() {
                self.paste(&text);
            }
            return DialogAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return DialogAction::None;
        }
        match self.stage {
            DialogStage::SelectProvider => self.handle_provider_key(key),
            DialogStage::NameInstance => self.handle_name_key(key),
            DialogStage::EnterCredential => self.handle_credential_key(key),
            DialogStage::Flow => self.handle_flow_key(key),
        }
    }

    /// Keys on the provider list: arrows choose, enter descends into the
    /// name input, esc closes the dialog.
    fn handle_provider_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let count = PROVIDERS.len() as isize;
                let step = if key.code == KeyCode::Down { 1 } else { -1 };
                self.selected = ((self.selected as isize + step).rem_euclid(count)) as usize;
                DialogAction::None
            }
            KeyCode::Enter => self.choose_provider(),
            KeyCode::Esc => DialogAction::Close,
            _ => DialogAction::None,
        }
    }

    /// Keys while naming the instance: enter accepts a non-blank name
    /// and descends into the credential input — or starts the provider's
    /// interactive flow, for the providers that log in that way — esc
    /// returns to the provider list.
    fn handle_name_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                DialogAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                DialogAction::None
            }
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    return DialogAction::None;
                }
                if let Some(pending) = self.pending.as_mut() {
                    pending.name = self.input.trim().to_owned();
                }
                self.input.clear();
                if self.pending.as_ref().map(|p| p.provider) == Some(LoginProvider::Copilot) {
                    // A flow provider has no key to type: the stage
                    // flips and the app spawns the flow.
                    self.status = DialogStatus::Validating;
                    self.flow_instruction = None;
                    self.flow_waiting = None;
                    self.stage = DialogStage::Flow;
                    return DialogAction::StartFlow;
                }
                self.second_input = false;
                self.stage = DialogStage::EnterCredential;
                DialogAction::None
            }
            KeyCode::Esc => {
                self.input.clear();
                self.status = DialogStatus::Idle;
                self.pending = None;
                self.stage = DialogStage::SelectProvider;
                DialogAction::None
            }
            _ => DialogAction::None,
        }
    }

    /// Keys while typing the credential: enter validates — after
    /// capturing the first prompt's URL for the providers that ask one —
    /// esc returns to the name input.
    fn handle_credential_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                DialogAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                DialogAction::None
            }
            KeyCode::Enter => {
                let provider = self.pending.as_ref().map(|p| p.provider);
                if !self.second_input
                    && matches!(
                        provider,
                        Some(LoginProvider::LlamaCpp | LoginProvider::OpenAi | LoginProvider::Zai)
                    )
                {
                    // The URL prompt: captured, then the key is asked.
                    if let Some(pending) = self.pending.as_mut() {
                        pending.base_url = Some(self.input.trim().to_owned());
                    }
                    self.second_input = true;
                    self.input.clear();
                    self.status = DialogStatus::Idle;
                    return DialogAction::None;
                }
                self.submit()
            }
            KeyCode::Esc => {
                self.input = self.pending_name();
                self.second_input = false;
                self.status = DialogStatus::Idle;
                self.stage = DialogStage::NameInstance;
                DialogAction::None
            }
            _ => DialogAction::None,
        }
    }

    /// Keys while a flow runs: esc cancels the dialog; everything else
    /// is inert — the flow has no input of its own.
    fn handle_flow_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Esc => DialogAction::CloseCancelFlow,
            _ => DialogAction::None,
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
            LoginProvider::Zai => "zai",
            LoginProvider::Copilot => "copilot",
        };
        if !self.instances.contains_key(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|name| !self.instances.contains_key(name))
            .unwrap_or_else(|| format!("{base}-{}", self.instances.len() + 1))
    }

    /// Enter on the final credential prompt: validate, unless a
    /// validation is already in flight. A llama.cpp bearer key may be
    /// blank (an open server); every other key must be filled.
    fn submit(&mut self) -> DialogAction {
        if matches!(self.status, DialogStatus::Validating) {
            return DialogAction::None;
        }
        let blank = self.input.trim().is_empty();
        let key_optional = self.second_input
            && self.pending.as_ref().map(|p| p.provider) == Some(LoginProvider::LlamaCpp);
        if blank && !key_optional {
            return DialogAction::None;
        }
        self.status = DialogStatus::Validating;
        DialogAction::Validate
    }

    /// Records the validation outcome: an error renders inline and
    /// allows retrying; success leaves the transition to the app (it
    /// needs the models and the store).
    pub fn on_validated(&mut self, result: &Result<Vec<ModelCard>, String>) {
        if let Err(message) = result {
            self.status = DialogStatus::Error(message.clone());
        }
    }
}

/// The settings screen: a sidebar of sections on the left, the selected
/// section's page on the right, and — over the Providers page — the
/// pop-in dialog that adds a login. The Providers page is live (its
/// instance list mirrors the config as it edits it); the other pages
/// read snapshots taken at open time. The appearance toggles write both
/// the app state and this screen so the page keeps telling the truth.
#[derive(Debug)]
pub struct SettingsScreen {
    /// The section whose page is showing.
    pub section: SettingsSection,
    /// The selected row within the page's interactive rows (the
    /// Providers page's instance list, the Appearance page's toggles).
    pub row: usize,
    /// Appearance: the color palette on screen.
    pub colors: crate::palette::Palette,
    /// Appearance: whether the dark theme is the one on screen.
    pub theme_dark: bool,
    /// Appearance: whether the icons are the Nerd Font set.
    pub icons_nerd: bool,
    // The Providers page — live.
    /// The profile this screen edits: the session's.
    pub profile: String,
    /// The profile's stored settings, synced from the config whenever
    /// the app mutates it.
    pub stored: rhema_config::Profile,
    /// The add-instance dialog, when it is up.
    pub dialog: Option<AddInstanceDialog>,
    // The Sandbox page's snapshot.
    /// What this machine offered at the startup probe: the rows'
    /// offered marks.
    pub sandbox_offered: crate::sandbox::SandboxAvailability,
    /// The kind this session's `bash` runs under.
    pub sandbox_active: crate::args::SandboxChoice,
    /// The kind this workspace last ran, when remembered.
    pub sandbox_remembered: Option<crate::args::SandboxChoice>,
    /// The add-sandbox dialog, when it is up.
    pub sandbox_dialog: Option<AddSandboxDialog>,
    // The Profiles page's snapshot.
    /// The config's profiles, sorted by name.
    pub profiles: Vec<ProfileRow>,
    /// The add-profile dialog, when it is up.
    pub add_profile: Option<AddProfileDialog>,
    // The Skills page's snapshot.
    /// The discovered skills, global and workspace merged.
    pub skills: Vec<SkillRow>,
    /// The global skills directory, when one resolves.
    pub skills_global_dir: Option<String>,
    /// The workspace's own skills directory.
    pub skills_workspace_dir: String,
    // The Subagents page's snapshot.
    /// The discovered subagent definitions, built-in and discovered
    /// merged.
    pub subagents: Vec<SubagentRow>,
    /// The global subagents directory, when one resolves.
    pub subagents_global_dir: Option<String>,
    /// The workspace's own subagents directory.
    pub subagents_workspace_dir: String,
    // The Usage page's snapshot.
    /// The active backend's subscription allowance, tagged with the
    /// backend's config name.
    pub allowance: Option<(&'static str, Option<rhema_provider::Allowance>)>,
    /// The last finished turn's token accounting.
    pub last_turn: Option<rhema_core::Usage>,
    /// The transcript's compaction benchmark, when the agent runs with
    /// the transcript on.
    pub transcript: Option<rhema_transcript::TranscriptStats>,
    /// The session's accumulated cost in USD.
    pub session_cost: f64,
    // The About page's snapshot.
    /// The config file's path, when one resolves.
    pub config_path: Option<String>,
    /// The workspace root.
    pub workspace: String,
    /// How many profiles the config holds.
    pub profile_count: usize,
}

impl Default for SettingsScreen {
    /// A settings screen on the Providers page: dark theme, plain
    /// icons, empty snapshots — the opener fills the rest.
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsScreen {
    /// A settings screen on the Providers page: dark theme, plain
    /// icons, empty snapshots — the opener fills the rest.
    pub fn new() -> Self {
        Self {
            section: SettingsSection::Providers,
            row: 0,
            colors: crate::palette::Palette::Rhema,
            theme_dark: true,
            icons_nerd: false,
            profile: "personal".to_owned(),
            stored: rhema_config::Profile::default(),
            dialog: None,
            sandbox_offered: crate::sandbox::SandboxAvailability::default(),
            sandbox_active: crate::args::SandboxChoice::Local,
            sandbox_remembered: None,
            sandbox_dialog: None,
            profiles: Vec::new(),
            add_profile: None,
            skills: Vec::new(),
            skills_global_dir: None,
            skills_workspace_dir: String::new(),
            subagents: Vec::new(),
            subagents_global_dir: None,
            subagents_workspace_dir: String::new(),
            allowance: None,
            last_turn: None,
            transcript: None,
            session_cost: 0.0,
            config_path: None,
            workspace: String::new(),
            profile_count: 0,
        }
    }

    /// Steps the sidebar one section, wrapping at the ends. The page's
    /// row selection restarts at the top: the rows move with the page.
    fn step_section(&mut self, forward: bool) {
        let count = SettingsSection::ALL.len() as isize;
        let current = SettingsSection::ALL
            .iter()
            .position(|section| *section == self.section)
            .unwrap_or(0) as isize;
        let step = if forward { 1 } else { -1 };
        self.section = SettingsSection::ALL[(current + step).rem_euclid(count) as usize];
        self.row = 0;
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

    /// The Sandbox page as rendered rows: the four kinds in the spec's
    /// canonical order, then the add row. The detail names the kind's
    /// stored fact; the marks say active, remembered, offered.
    pub fn sandbox_rows(&self) -> Vec<SandboxRow<'_>> {
        let sandbox = self.stored.sandbox.as_ref();
        let active_name = sandbox.and_then(|s| s.active.as_deref());
        // The host's own row is pinned: the local instance exists by
        // doing nothing, and every stored instance lists after it, in
        // the map's name order.
        let mut rows = vec![SandboxRow {
            name: "local",
            kind: crate::args::SandboxChoice::Local,
            detail: "this host".to_owned(),
            offered: true,
            remembered: self.sandbox_remembered == Some(crate::args::SandboxChoice::Local),
            active: self.sandbox_active == crate::args::SandboxChoice::Local,
            add: false,
        }];
        rows.extend(
            self.stored
                .sandbox_instances
                .iter()
                .map(|(name, instance)| {
                    let kind = crate::args::SandboxChoice::from_stored(Some(&instance.kind));
                    let vm = instance.vm.unwrap_or(false) && instance.kind == "lxd";
                    SandboxRow {
                        name,
                        kind: kind.unwrap_or(crate::args::SandboxChoice::Local),
                        detail: match (instance.target.as_deref(), vm) {
                            (Some(target), false) => target.to_owned(),
                            (Some(target), true) => format!("{target} · vm"),
                            (None, _) => "no target set".to_owned(),
                        },
                        offered: kind
                            .map(|kind| self.sandbox_offered.offers(kind))
                            .unwrap_or(false),
                        remembered: kind == self.sandbox_remembered,
                        active: active_name == Some(name.as_str()),
                        add: false,
                    }
                })
                .collect::<Vec<_>>(),
        );
        rows.push(SandboxRow {
            name: "",
            kind: crate::args::SandboxChoice::Local,
            detail: String::new(),
            offered: false,
            remembered: false,
            active: false,
            add: true,
        });
        rows
    }

    /// The name of the highlighted instance row; `None` on the add row
    /// or anywhere but the Providers page.
    pub fn selected_instance(&self) -> Option<&str> {
        if self.section != SettingsSection::Providers || self.dialog.is_some() {
            return None;
        }
        self.instance_rows()
            .get(self.row)
            .and_then(|row| (!row.add).then_some(row.name))
    }

    /// The highlighted profile row; `None` on the add row, with the
    /// dialog up, or anywhere but the Profiles page.
    pub fn selected_profile(&self) -> Option<&str> {
        if self.section != SettingsSection::Profiles || self.add_profile.is_some() {
            return None;
        }
        self.profiles.get(self.row).map(|row| row.name.as_str())
    }

    /// The highlighted local row; `Some` only there — instance rows
    /// answer [`Self::selected_sandbox_instance`] instead, and the add
    /// row answers neither.
    pub fn selected_sandbox(&self) -> Option<crate::args::SandboxChoice> {
        if self.section != SettingsSection::Sandbox || self.sandbox_dialog.is_some() {
            return None;
        }
        self.sandbox_rows()
            .get(self.row)
            .and_then(|row| (row.name == "local").then_some(row.kind))
    }

    /// The highlighted sandbox instance row's name; `None` on the
    /// local and add rows, with the dialog up, or anywhere but the
    /// Sandbox page.
    pub fn selected_sandbox_instance(&self) -> Option<&str> {
        if self.section != SettingsSection::Sandbox || self.sandbox_dialog.is_some() {
            return None;
        }
        self.sandbox_rows()
            .get(self.row)
            .and_then(|row| (!row.add && row.name != "local").then_some(row.name))
    }

    /// The highlighted subagent row; `None` anywhere but the Subagents
    /// page, with the dialog up, or past the list's end.
    pub fn selected_subagent(&self) -> Option<&SubagentRow> {
        if self.section != SettingsSection::Subagents || self.dialog.is_some() {
            return None;
        }
        self.subagents.get(self.row)
    }

    /// The highlighted skills row; `None` anywhere but the Skills page,
    /// with the dialog up, or past the list's end.
    pub fn selected_skill(&self) -> Option<&SkillRow> {
        if self.section != SettingsSection::Skills || self.dialog.is_some() {
            return None;
        }
        self.skills.get(self.row)
    }

    /// How many interactive rows the page holds; zero means the arrows
    /// have nothing to move through.
    fn row_count(&self) -> usize {
        match self.section {
            SettingsSection::Providers => self.instance_rows().len(),
            SettingsSection::Profiles => self.profiles.len() + 1,
            SettingsSection::Sandbox => self.sandbox_rows().len(),
            SettingsSection::Appearance => 3,
            SettingsSection::Skills => self.skills.len(),
            SettingsSection::Subagents => self.subagents.len(),
            _ => 0,
        }
    }

    /// Moves the row selection among the page's rows, wrapping.
    fn move_row(&mut self, down: bool) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let count = count as isize;
        let step = if down { 1 } else { -1 };
        self.row = ((self.row as isize + step).rem_euclid(count)) as usize;
    }

    /// What Enter (or a click) does on the page's selected row.
    pub fn enter_action(&self) -> SettingsAction {
        match self.section {
            SettingsSection::Providers => match self.selected_instance() {
                Some(_) => SettingsAction::ActivateInstance,
                None => SettingsAction::OpenDialog,
            },
            SettingsSection::Profiles => match self.selected_profile() {
                Some(_) => SettingsAction::SelectProfile,
                None => SettingsAction::OpenProfileDialog,
            },
            SettingsSection::Appearance => match self.row {
                0 => SettingsAction::CyclePalette,
                1 => SettingsAction::ToggleTheme,
                _ => SettingsAction::ToggleIcons,
            },
            SettingsSection::Subagents => match self.selected_subagent() {
                Some(_) => SettingsAction::EditSubagent,
                None => SettingsAction::None,
            },
            SettingsSection::Sandbox => {
                if self.selected_sandbox_instance().is_some() {
                    SettingsAction::SelectSandboxInstance
                } else {
                    match self.selected_sandbox() {
                        Some(choice) => SettingsAction::SelectSandbox(choice),
                        None => SettingsAction::OpenSandboxDialog,
                    }
                }
            }
            SettingsSection::Skills => match self.selected_skill() {
                Some(_) => SettingsAction::ToggleSkill,
                None => SettingsAction::None,
            },
            _ => SettingsAction::None,
        }
    }

    /// Handles one keypress. The left/right arrows and tabs move the
    /// sidebar, the vertical arrows move the page's rows, esc or `q`
    /// backs out (dialog first, then the screen), enter acts on the
    /// selected row, `d` forgets an instance or removes a profile, `c`
    /// cycles a profile's name color (and colors the Appearance page),
    /// `e` edits the selected subagent, and the Appearance page carries
    /// `t`/`i` as shortcuts for its remaining toggles.
    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        if let Some(action) = self
            .add_profile
            .as_mut()
            .map(|dialog| dialog.handle_key(key))
        {
            if action == ProfileDialogAction::Close {
                self.add_profile = None;
            }
            return match action {
                ProfileDialogAction::None => SettingsAction::None,
                ProfileDialogAction::Submit => SettingsAction::SubmitProfileDialog,
                ProfileDialogAction::Close => SettingsAction::CloseProfileDialog,
            };
        }
        if let Some(action) = self.dialog.as_mut().map(|dialog| dialog.handle_key(key)) {
            // Esc closes the dialog from any stage; the app drops the
            // flow task's handle when it sees the CloseDialog action.
            if matches!(action, DialogAction::Close | DialogAction::CloseCancelFlow) {
                self.dialog = None;
            }
            return match action {
                DialogAction::None => SettingsAction::None,
                DialogAction::Validate => SettingsAction::Validate,
                DialogAction::StartFlow => SettingsAction::StartFlow,
                DialogAction::Close | DialogAction::CloseCancelFlow => SettingsAction::CloseDialog,
            };
        }
        if let Some(action) = self
            .sandbox_dialog
            .as_mut()
            .map(|dialog| dialog.handle_key(key))
        {
            if action == SandboxDialogAction::Close {
                self.sandbox_dialog = None;
            }
            return match action {
                SandboxDialogAction::None => SettingsAction::None,
                SandboxDialogAction::Submit => SettingsAction::SubmitSandboxDialog,
                SandboxDialogAction::Close => SettingsAction::CloseSandboxDialog,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return SettingsAction::None;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.step_section(false);
                SettingsAction::None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.step_section(true);
                SettingsAction::None
            }
            KeyCode::Up => {
                self.move_row(false);
                SettingsAction::None
            }
            KeyCode::Down => {
                self.move_row(true);
                SettingsAction::None
            }
            KeyCode::Esc | KeyCode::Char('q') => SettingsAction::Back,
            KeyCode::Enter => self.enter_action(),
            KeyCode::Char('d') if self.selected_instance().is_some() => {
                SettingsAction::RemoveInstance
            }
            KeyCode::Char('d') if self.selected_profile().is_some() => {
                SettingsAction::RemoveProfile
            }
            KeyCode::Char('d') if self.selected_sandbox_instance().is_some() => {
                SettingsAction::RemoveSandboxInstance
            }
            KeyCode::Char('c') if self.selected_profile().is_some() => {
                SettingsAction::CycleProfileColor
            }
            KeyCode::Char('e') if self.selected_subagent().is_some() => {
                SettingsAction::EditSubagent
            }
            KeyCode::Char(' ' | 't') if self.selected_skill().is_some() => {
                SettingsAction::ToggleSkill
            }
            KeyCode::Char('c') if self.section == SettingsSection::Appearance => {
                SettingsAction::CyclePalette
            }
            KeyCode::Char('t') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleTheme
            }
            KeyCode::Char('i') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleIcons
            }
            _ => SettingsAction::None,
        }
    }
}

/// What one add-profile dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileDialogAction {
    /// Nothing to do.
    None,
    /// Enter on a fresh non-blank name: the app creates the profile.
    Submit,
    /// Esc left the dialog.
    Close,
}

/// The pop-in dialog over the Profiles page: one line naming the profile
/// to create. Creation is the app's; the dialog collects the name and
/// refuses the names the registry already holds.
#[derive(Debug, Default)]
pub struct AddProfileDialog {
    /// The line being typed.
    pub input: String,
    /// The profile names taken when the dialog opened. The dialog is
    /// modal, so a snapshot cannot go stale.
    taken: BTreeSet<String>,
    /// Why the last enter did not submit, when it did not: the text the
    /// status line renders.
    pub error: Option<String>,
}

impl AddProfileDialog {
    /// An empty dialog, deduping names against the config's registry.
    pub fn new(profiles: &BTreeMap<String, rhema_config::Profile>) -> Self {
        Self {
            input: String::new(),
            taken: profiles.keys().cloned().collect(),
            error: None,
        }
    }

    /// The typed name, trimmed; blank names never submit.
    pub fn name(&self) -> &str {
        self.input.trim()
    }

    /// A bracketed paste landed on the input. A name is a single line:
    /// control characters — newlines above all, the trailing one a
    /// stray copy loves to carry — are dropped.
    pub fn paste(&mut self, text: &str) {
        self.input.extend(text.chars().filter(|c| !c.is_control()));
    }

    /// Handles one keypress. Printable keys and backspace edit, enter
    /// submits a fresh non-blank name, esc closes; ctrl-v (ctrl-shift-v
    /// included) reads the clipboard, like the add-instance dialog's
    /// inputs.
    pub fn handle_key(&mut self, key: KeyEvent) -> ProfileDialogAction {
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = crate::clipboard::read() {
                self.paste(&text);
            }
            return ProfileDialogAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ProfileDialogAction::None;
        }
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                self.error = None;
                ProfileDialogAction::None
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.error = None;
                ProfileDialogAction::None
            }
            KeyCode::Enter => {
                let name = self.name().to_owned();
                if name.is_empty() {
                    return ProfileDialogAction::None;
                }
                if self.taken.contains(&name) {
                    self.error = Some(format!("a profile named {name} already exists"));
                    return ProfileDialogAction::None;
                }
                ProfileDialogAction::Submit
            }
            KeyCode::Esc => ProfileDialogAction::Close,
            _ => ProfileDialogAction::None,
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

/// The command a slash input names together with the fixed words its
/// argument completes from, when the command's argument is one:
/// `/thinking h` names `/thinking` and its levels. None when the input
/// names no command, or one whose argument completes from live rows
/// (the models, the profiles).
fn choice_argument(input: &str) -> Option<(&'static str, &'static [crate::commands::Choice])> {
    let body = input.strip_prefix('/')?;
    let (name, _) = body.split_once(' ')?;
    let spec = crate::commands::COMMANDS
        .iter()
        .find(|spec| &spec.name[1..] == name)?;
    match spec.argument {
        crate::commands::Argument::Choice(choices) => Some((spec.name, choices)),
        _ => None,
    }
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
    /// A fixed word under a command that names one (`/thinking`);
    /// completing fills the argument.
    Choice {
        /// The command the word belongs to, with the slash.
        command: &'static str,
        /// The word completion inserts.
        value: &'static str,
        /// What the row says about the word, after it.
        description: &'static str,
    },
    /// A filesystem path under the cursor; completing splices the
    /// entry over the token it came from.
    Path {
        /// Start of the token (char offset) the completion replaces.
        token_start: usize,
        /// End of the token (char offset), exclusive.
        token_end: usize,
        /// The entry's name, with a trailing `/` on directories — the
        /// row's label.
        name: String,
        /// Whether the entry is a directory.
        is_dir: bool,
        /// The text to splice over the token: the directory part as
        /// typed, the entry's name, and a trailing `/` on directories.
        completion: String,
    },
}

/// The palette's rows for an input: command names while the input
/// matches one, the models matching the `/model` argument, the profiles
/// matching the `/profile` argument, or — when none of those fire — the
/// filesystem entries under the cursor's path token. The argument
/// filters follow the picker's rules — the rendered row text,
/// case-insensitive.
pub fn palette_rows(
    input: &str,
    cursor: usize,
    models: &[CatalogModel],
    profiles: &[PaletteProfile],
    icons: &crate::icons::Icons,
) -> Vec<PaletteRow> {
    if !input.starts_with('/') {
        return path_completion_rows(input, cursor);
    }
    if let Some(needle) = input.strip_prefix("/model ") {
        let needle = needle.to_lowercase();
        let rows: Vec<PaletteRow> = models
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
                description: model_description(&entry.model, icons),
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    if let Some(needle) = input.strip_prefix("/profile ") {
        let needle = needle.to_lowercase();
        let rows: Vec<PaletteRow> = profiles
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
        if !rows.is_empty() {
            return rows;
        }
    }
    // A choice argument lists its command's fixed words, filtered by
    // the half-typed one: `/thinking h` matches `high`.
    if let Some((command, choices)) = choice_argument(input) {
        let needle = input
            .split_once(' ')
            .map(|(_, rest)| rest.trim().to_lowercase())
            .unwrap_or_default();
        let rows: Vec<PaletteRow> = choices
            .iter()
            .filter(|choice| needle.is_empty() || choice.name.starts_with(&needle))
            .map(|choice| PaletteRow::Choice {
                command,
                value: choice.name,
                description: choice.description,
            })
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    let commands: Vec<PaletteRow> = filtered_commands(input)
        .into_iter()
        .map(PaletteRow::Command)
        .collect();
    if !commands.is_empty() {
        return commands;
    }
    // A slash input naming no command may still be a path: `/etc/hos`
    // completes like any other absolute path.
    path_completion_rows(input, cursor)
}

/// The most path rows one token lists: big directories still complete,
/// without the palette carrying every entry.
const MAX_PATH_ROWS: usize = 256;

/// The path rows for the token under the cursor: the listed directory's
/// entries whose name starts with the typed stem, case-insensitive,
/// directories first. Hidden entries (a leading dot) stay hidden until
/// the stem leads with one. A token completes nothing unless it carries
/// a separator or opens with `~` or `.`; a directory that cannot be
/// read simply lists nothing.
fn path_completion_rows(input: &str, cursor: usize) -> Vec<PaletteRow> {
    let Some((token_start, token_end)) = path_token(input, cursor) else {
        return Vec::new();
    };
    let token: String = input
        .chars()
        .skip(token_start)
        .take(token_end - token_start)
        .collect();
    let Some((dir_part, stem, dir)) = split_path_token(&token) else {
        return Vec::new();
    };
    let needle = stem.to_lowercase();
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|listing| {
            listing
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    if name.starts_with('.') && !stem.starts_with('.') {
                        return None;
                    }
                    let is_dir = entry.path().is_dir();
                    (needle.is_empty() || name.to_lowercase().starts_with(&needle))
                        .then_some((name, is_dir))
                })
                .collect()
        })
        .unwrap_or_default();
    // Directories first, then names case-insensitively.
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    entries.truncate(MAX_PATH_ROWS);
    entries
        .into_iter()
        .map(|(name, is_dir)| PaletteRow::Path {
            token_start,
            token_end,
            name: if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            },
            is_dir,
            completion: format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }),
        })
        .collect()
}

/// The word under the cursor: the run of characters around it that are
/// neither whitespace nor an attachment anchor, as `(start, end)` char
/// offsets. None when the run is empty.
fn path_token(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let breaks = |c: char| c.is_whitespace() || c == ChatState::MARK;
    let chars: Vec<char> = input.chars().collect();
    let pos = cursor.min(chars.len());
    let start = (0..pos)
        .rev()
        .find(|index| breaks(chars[*index]))
        .map_or(0, |index| index + 1);
    let end = chars[pos..]
        .iter()
        .position(|c| breaks(*c))
        .map_or(chars.len(), |offset| pos + offset);
    (start < end).then_some((start, end))
}

/// The token split for completion: the directory part as typed (through
/// the final `/`), the stem the entries must match, and the directory
/// to list — `~` expands to the home directory, an empty part to the
/// working directory. None when the token names no path.
fn split_path_token(token: &str) -> Option<(String, String, PathBuf)> {
    if token == "~" {
        let home = std::env::home_dir()?;
        return Some(("~".to_owned(), String::new(), home));
    }
    let path_like = token.starts_with('~') || token.starts_with('.') || token.contains('/');
    if !path_like {
        return None;
    }
    let (dir_part, stem) = match token.rfind('/') {
        Some(index) => (&token[..=index], &token[index + 1..]),
        None => ("", token),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        std::env::home_dir()?.join(rest)
    } else if dir_part == "~/" {
        std::env::home_dir()?
    } else if dir_part.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(dir_part)
    };
    Some((dir_part.to_owned(), stem.to_owned(), dir))
}

/// A model's palette description: the display name with the picker's
/// facts — pricing, the vision verdict, and the context window — when
/// the catalog knows them. The verdict is tri-state like the catalog's:
/// `None` says nothing, because unknown is not a verdict.
fn model_description(model: &ModelCard, icons: &crate::icons::Icons) -> String {
    let mut detail = model.display_name.clone();
    if let Some(pricing) = model.pricing {
        detail.push_str(&format!(
            " · ${:.2}/M in · ${:.2}/M out",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    match model.supports_vision {
        Some(true) => detail.push_str(&format!(" · {} vision", icons.vision)),
        Some(false) => detail.push_str(" · no vision"),
        None => {}
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
}

impl PickerScreen {
    /// A picker over `models` (text chat models only, sorted by provider
    /// then id), initially unfiltered.
    pub fn new(mut models: Vec<CatalogModel>) -> Self {
        models.retain(|entry| Self::usable(&entry.model));
        models.sort_by(|a, b| a.provider.cmp(b.provider).then(a.model.id.cmp(&b.model.id)));
        Self {
            models,
            failed: Vec::new(),
            filter: String::new(),
            selected: 0,
            status: PickerStatus::Ready,
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
    /// extend the filter, arrows navigate, enter picks. Esc always
    /// leaves — the filter is transient, nothing to clear first — and
    /// returns to the chat. `q` stays a filter character on purpose:
    /// models are named (qwen).
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
                KeyCode::Esc => return PickerAction::BackToChat,
                _ => return PickerAction::None,
            }
        }
        // Loading or failed: only retry and leave are meaningful.
        match key.code {
            KeyCode::Char('r') if matches!(self.status, PickerStatus::Error(_)) => {
                self.status = PickerStatus::Loading;
                PickerAction::Retry
            }
            KeyCode::Esc => PickerAction::BackToChat,
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
#[derive(Debug, Clone, PartialEq)]
pub enum ChatAction {
    /// Nothing to do.
    None,
    /// Enter on the (possibly empty) input; the app drops empty and
    /// suppresses submits while a turn runs. The text is the session
    /// form — paste payloads embedded at their anchors — which the app
    /// pushes to the transcript as-is and expands only where plain
    /// text is wanted (commands, the provider). The images ride beside
    /// the text, in their anchors' order.
    Submit {
        /// The session-form text.
        text: String,
        /// Images submitted beside the text.
        images: Vec<rhema_core::Image>,
    },
}

/// One chip anchored in the input: the payload a [`ChatState::MARK`]
/// carries. Text pastes and clipboard images anchor the same way and
/// render the same — one atomic cell — and differ only in what submit
/// does with them.
#[derive(Debug, Clone, PartialEq)]
pub enum Chip {
    /// A multi-line text paste's payload.
    Paste(String),
    /// A clipboard image, carried as bare base64.
    Image(rhema_core::Image),
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
    /// The anchors' payloads, in the order their [`Self::MARK`] anchors
    /// appear in `input` — a 1:1 pairing the edit paths keep true. Chips
    /// render at their anchors; submit embeds the paste payloads in
    /// place and hands the images over beside the text.
    pub chips: Vec<Chip>,
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
    /// removes the chip and its payload together. Only [`Self::paste`]
    /// mints it, always paired with a payload — text arriving with
    /// marks of its own loses them, so a chip glyph copied back out of
    /// the box can never anchor empty.
    pub const MARK: char = rhema_core::paste::MARK;

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
    /// it becomes a chip anchored at the cursor — one cell in the box at
    /// the paste's place, full content on submit.
    pub fn paste(&mut self, text: &str) {
        let normalized = Self::normalize(text);
        if normalized.lines().count() > Self::PASTE_INLINE_LINES {
            self.chips.push(Chip::Paste(normalized));
            self.insert_char(Self::MARK);
        } else {
            self.insert_str(&normalized);
        }
    }

    /// A clipboard image landed: always a chip anchored at the cursor,
    /// never inline — an image has no inline form, and a prompt carries
    /// it as one cell beside the text, full payload on submit.
    pub fn paste_image(&mut self, image: rhema_core::Image) {
        self.chips.push(Chip::Image(image));
        self.insert_char(Self::MARK);
    }

    /// Terminal text made insertable: carriage returns normalize to
    /// newlines, and anchor marks are dropped — a mark is only ever
    /// minted by [`Self::paste`], paired with a payload.
    fn normalize(text: &str) -> String {
        rhema_core::paste::normalize(text)
    }

    /// Inserts one character at the cursor.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.byte_offset(), c);
        self.cursor += 1;
        self.palette_index = 0;
    }

    /// Removes `input[start..end]` (byte offsets) and, with it, the
    /// chip of every anchor in the range, paste or image — the 1:1
    /// mark-to-chip pairing survives any deletion.
    fn delete_range(&mut self, start: usize, end: usize) {
        let dropped = self.input[start..end].matches(Self::MARK).count();
        if dropped > 0 {
            let first = self.input[..start].matches(Self::MARK).count();
            self.chips.drain(first..first + dropped);
        }
        self.input.replace_range(start..end, "");
    }

    /// What a submit sends: the typed input with every paste embedded at
    /// its anchor, and the images beside it in anchor order — the
    /// session's record of the prompt. The transcript renders each embed
    /// as its chip, and the model sees the plain expansion
    /// (`rhema_core::paste::expand`, applied at the provider seam), never
    /// the encoding. Image anchors drop out of the text: an image
    /// travels as a block beside it, and an anchor left in would desync
    /// `embed`'s positional pairing of marks to payloads.
    fn submission(&self) -> (String, Vec<rhema_core::Image>) {
        let mut marked = String::with_capacity(self.input.len());
        let mut pastes = Vec::new();
        let mut images = Vec::new();
        for (index, piece) in self.input.split(Self::MARK).enumerate() {
            if index > 0 {
                match self.chips.get(index - 1) {
                    Some(Chip::Paste(payload)) => {
                        pastes.push(payload.clone());
                        marked.push(Self::MARK);
                    }
                    Some(Chip::Image(image)) => images.push(image.clone()),
                    None => {}
                }
            }
            marked.push_str(piece);
        }
        (rhema_core::paste::embed(&marked, &pastes), images)
    }

    /// Inserts `text` at the cursor, newlines and all — the bracketed
    /// paste path. Carriage returns normalize to newlines, since a
    /// terminal paste may carry either.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = Self::normalize(text);
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
    /// empty, and the filesystem feeds path rows for the token under
    /// the cursor.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        models: &[CatalogModel],
        profiles: &[PaletteProfile],
        icons: &'static crate::icons::Icons,
    ) -> ChatAction {
        // Ctrl-J is readline's historical newline: it inserts even
        // though plain control combos are inert.
        if key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL {
            self.insert_newline();
            return ChatAction::None;
        }
        // Ctrl-v (with or without shift) is paste in terminals that do
        // not bind it themselves: the clipboard is read directly, the
        // same content a bracketed paste would have carried. A clipboard
        // holds one thing or the other — an image is tried first and the
        // text falls through.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            if let Some(image) = crate::clipboard::read_image() {
                self.paste_image(image);
            } else if let Some(text) = crate::clipboard::read() {
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
                // A terminal without bracketed paste delivers a paste as
                // plain char keys: a mark arriving this way is a copied
                // chip glyph, and must not anchor without a payload.
                if c == Self::MARK {
                    return ChatAction::None;
                }
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
            // The palette owns the vertical arrows while it has rows —
            // commands, models under `/model`, profiles under
            // `/profile`, or the path entries under the cursor's token.
            KeyCode::Up | KeyCode::Down
                if !palette_rows(&self.input, self.cursor, models, profiles, icons).is_empty() =>
            {
                let count = palette_rows(&self.input, self.cursor, models, profiles, icons).len();
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
                let rows = palette_rows(&self.input, self.cursor, models, profiles, icons);
                if let Some(row) = rows.get(self.palette_index.min(rows.len().saturating_sub(1))) {
                    match row {
                        PaletteRow::Command(spec) => {
                            // The input is replaced wholesale: any
                            // anchors in it go, and their payloads go
                            // with them.
                            self.chips.clear();
                            self.input = spec.name.to_owned();
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Model { id, .. } => {
                            self.chips.clear();
                            self.input = format!("/model {id}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Profile { name, .. } => {
                            self.chips.clear();
                            self.input = format!("/profile {name}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Choice { command, value, .. } => {
                            self.chips.clear();
                            self.input = format!("{command} {value}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Path {
                            token_start,
                            token_end,
                            completion,
                            ..
                        } => {
                            // A local splice: anchors outside the token
                            // keep their payloads, and delete_range
                            // drops any inside it.
                            self.cursor = (*token_end).min(self.input.chars().count());
                            let end = self.byte_offset();
                            self.cursor = *token_start;
                            let start = self.byte_offset();
                            self.delete_range(start, end);
                            self.insert_str(completion);
                        }
                    }
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
                let (text, images) = self.submission();
                ChatAction::Submit { text, images }
            }
            _ => ChatAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
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
        // takes every key while it is open.
        assert_eq!(
            settings.handle_key(key(KeyCode::Enter)),
            SettingsAction::None
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
    fn the_sandbox_dialog_picks_a_kind_then_collects_a_value() {
        let mut settings = sandbox_screen(
            SandboxAvailability::default(),
            None,
            SandboxChoice::Local,
            rhema_config::Profile::default(),
        );
        settings.sandbox_dialog = Some(AddSandboxDialog::new());
        // Docker is the kind list's only row; enter takes it.
        settings.handle_key(key(KeyCode::Enter));
        let dialog = settings.sandbox_dialog.as_ref().expect("still up");
        assert_eq!(dialog.kind, SandboxChoice::Docker);
        // A blank value never submits.
        settings.handle_key(key(KeyCode::Enter));
        assert!(settings.sandbox_dialog.is_some());
        // The value types; esc walks back to the kind list and the
        // next pick clears the line.
        for c in "box@far".chars() {
            settings.handle_key(key(KeyCode::Char(c)));
        }
        settings.handle_key(key(KeyCode::Esc));
        let dialog = settings.sandbox_dialog.as_ref().expect("still up");
        assert_eq!(dialog.stage, SandboxDialogStage::SelectKind);
        assert!(dialog.input.is_empty());
        settings.handle_key(key(KeyCode::Enter));
        for c in "rhema-dev".chars() {
            settings.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            settings.handle_key(key(KeyCode::Enter)),
            SettingsAction::SubmitSandboxDialog
        );
        let dialog = settings.sandbox_dialog.as_ref().expect("still up");
        assert_eq!(
            dialog.submitted(),
            (SandboxChoice::Docker, "rhema-dev".to_owned())
        );
        // The screen keeps the dialog until the app acts on the
        // submit; here esc walks back to the kind list, and a second
        // esc closes the dialog.
        settings.handle_key(key(KeyCode::Esc));
        assert_eq!(
            settings.sandbox_dialog.as_ref().expect("still up").stage,
            SandboxDialogStage::SelectKind
        );
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        chat.handle_key(key(KeyCode::Left), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Left), &[], &[], &crate::icons::UNICODE);
        // cursor sits between "hel" and "o"
        chat.handle_key(key(KeyCode::Char('l')), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "hello");
        assert_eq!(chat.cursor, 3);
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "helo");
        chat.handle_key(key(KeyCode::End), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Char('!')), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "helo!");
        chat.handle_key(key(KeyCode::Home), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Delete), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "elo!");
    }

    #[test]
    fn chat_enter_submits_the_input_text() {
        let mut chat = ChatState::new();
        for c in "fix it".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
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
            &crate::icons::UNICODE,
        );
        assert_eq!(chat.input, "x\n");
        chat.handle_key(ctrl('j'), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "x\n\n");
        assert_eq!(chat.cursor, 3);
        // Enter itself still submits the whole multi-line text.
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
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
            &crate::icons::UNICODE,
        );
        assert_eq!(chat.input, "@");
        // Plain Ctrl combos stay inert.
        chat.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
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
        chat.handle_key(key(KeyCode::Up), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.cursor_line_index(), 1);
        assert_eq!(chat.cursor_column(), 4, "column kept");
        chat.handle_key(key(KeyCode::Up), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.cursor_line_index(), 0);
        assert_eq!(chat.cursor_column(), 4, "clamped to the shorter line");
        chat.handle_key(key(KeyCode::Up), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.cursor_line_index(), 0, "the top stays");
        chat.handle_key(key(KeyCode::Down), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Down), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.cursor_line_index(), 2);
    }

    #[test]
    fn home_and_end_stay_within_the_cursor_line() {
        let mut chat = ChatState::new();
        chat.insert_str("alpha\nbeta");
        chat.handle_key(key(KeyCode::Home), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.cursor_line_index(), 1);
        assert_eq!(chat.cursor_column(), 0);
        chat.handle_key(key(KeyCode::End), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
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
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
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
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
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
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, ChatState::MARK.to_string());
        assert!(matches!(chat.chips.as_slice(), [Chip::Image(_)]));

        // And the image chip deletes like any other.
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
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
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.chips, vec![Chip::Paste(first.to_owned())]);
        assert_eq!(chat.input, ChatState::MARK.to_string());

        // Deleting the remaining anchor from before it works the same.
        chat.handle_key(key(KeyCode::Home), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Delete), &[], &[], &crate::icons::UNICODE);
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
            &crate::icons::UNICODE,
        );
        assert_eq!(chat.input, "ab");

        // A real chip survives beside the stripped text: backspace
        // still removes the last payload, and nothing else.
        chat.paste("l1\nl2\nl3\nl4");
        assert_eq!(chat.input, format!("ab{}", ChatState::MARK));
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
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
            &crate::icons::UNICODE,
        );
        assert_eq!(chat.input, "A");
        // Ghostty & co. send the shifted char with SHIFT cleared
        // (crossterm's alternate-key path): untouched.
        chat.handle_key(key(KeyCode::Char('B')), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "AB");
        // Plain lowercase typing stays lowercase.
        chat.handle_key(key(KeyCode::Char('c')), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE),
            submit("")
        );
    }

    #[test]
    fn chat_scroll_moves_with_page_keys_and_never_goes_negative() {
        let mut chat = ChatState::new();
        chat.handle_key(key(KeyCode::PageUp), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::PageUp), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP);
        chat.scroll_by(-3);
        assert_eq!(chat.scroll, 2 * ChatState::SCROLL_STEP - 3);
        chat.handle_key(key(KeyCode::PageDown), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::PageDown), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::PageDown), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.scroll, 0);
        chat.scroll_by(5);
        chat.handle_key(key(KeyCode::Enter), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        assert_eq!(
            crate::screens::filtered_commands("/").len(),
            crate::commands::COMMANDS.len()
        );
        for c in "cl".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        chat.handle_key(key(KeyCode::Down), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Down), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.palette_index, 2);
        chat.handle_key(key(KeyCode::Up), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.palette_index, 1);
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Char(c)), &models, &[], &crate::icons::UNICODE);
        }
        // The empty argument lists the whole catalog; the provider filter
        // narrows like the picker's.
        let ids: Vec<String> = crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &models,
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
            chat.handle_key(key(KeyCode::Char(c)), &models, &[], &crate::icons::UNICODE);
        }
        let ids: Vec<String> = crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &models,
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
            &crate::icons::UNICODE,
        );
        let ids: Vec<String> = crate::screens::palette_rows(
            &chat.input,
            chat.cursor,
            &models,
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
        chat.handle_key(key(KeyCode::Tab), &models, &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "/model openrouter/auto");
        assert_eq!(chat.cursor, "/model openrouter/auto".chars().count());
    }

    #[test]
    fn the_thinking_palette_lists_its_levels_and_tab_fills_them() {
        let mut chat = ChatState::new();
        for c in "/thinking ".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        // The empty argument lists every level; a stem narrows it.
        let values = |chat: &ChatState| {
            crate::screens::palette_rows(&chat.input, chat.cursor, &[], &[], &crate::icons::UNICODE)
                .into_iter()
                .filter_map(|row| match row {
                    crate::screens::PaletteRow::Choice { value, .. } => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(values(&chat), ["low", "medium", "high", "default"]);
        for c in "hi".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        assert_eq!(values(&chat), ["high"]);
        // Tab completes the level, keeping the command prefix.
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, "/thinking high");
        assert_eq!(chat.cursor, "/thinking high".chars().count());
        // A stem no level answers leaves the palette empty.
        let mut chat = ChatState::new();
        for c in "/thinking zzz".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        assert!(
            crate::screens::palette_rows(
                &chat.input,
                chat.cursor,
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        assert!(matches!(
            crate::screens::palette_rows(&chat.input, chat.cursor, &[], &[], &crate::icons::UNICODE).as_slice(),
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
        chat.handle_key(key(KeyCode::Tab), &[], &profiles, &crate::icons::UNICODE);
        assert_eq!(chat.input, "/profile work");
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
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, format!("see {base}/Beta.txt"));
        assert_eq!(chat.cursor, chat.input.chars().count());
        // A directory gains its separator; the next Tab lists inside it.
        chat.input = format!("see {base}/su");
        chat.cursor = chat.input.chars().count();
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
        assert_eq!(chat.input, format!("see {base}/sub/"));
        assert_eq!(chat.cursor, chat.input.chars().count());
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        chat.handle_key(key(KeyCode::Tab), &[], &[], &crate::icons::UNICODE);
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
        let rows = crate::screens::palette_rows("/", 1, &[], &[], &crate::icons::UNICODE);
        assert!(rows.iter().all(|row| matches!(row, PaletteRow::Command(_))));
        // A `/model` argument with no catalog match falls through to
        // path completion.
        let input = format!("/model {base}/al");
        let rows = crate::screens::palette_rows(
            &input,
            input.chars().count(),
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
            chat.handle_key(key(KeyCode::Char(c)), &[], &[], &crate::icons::UNICODE);
        }
        chat.handle_key(key(KeyCode::Down), &[], &[], &crate::icons::UNICODE);
        chat.handle_key(key(KeyCode::Backspace), &[], &[], &crate::icons::UNICODE);
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
}
