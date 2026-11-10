//! The dialogs: the sandbox add, the instance add and its stages, and the profile add — the state machines behind the forms.

use super::*;

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
    crate::args::SandboxChoice::Ecrou,
];

/// The pop-in dialog over the Sandbox page: pick the kind and be
/// done. No target is asked — the concrete name is the workspace's
/// to set, derived at assembly time and stable across sessions, so
/// the stored instance is "docker for whatever workspace I am in".
/// Naming an existing instance — pinning a container, an LXD VM, a
/// host — is config.yaml's edit. Storing is the app's; the dialog
/// collects nothing else.
#[derive(Debug)]
pub struct AddSandboxDialog {
    /// The highlighted row of the kind list.
    pub selected: usize,
}

impl AddSandboxDialog {
    /// A dialog at the kind list.
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    /// The kind the highlighted row stands for.
    pub fn selected_kind(&self) -> crate::args::SandboxChoice {
        SANDBOX_ADD_KINDS[self.selected.min(SANDBOX_ADD_KINDS.len() - 1)]
    }

    /// The accepted kind; the app calls this on the Submit action.
    pub fn picked(&self) -> crate::args::SandboxChoice {
        self.selected_kind()
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> SandboxDialogAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return SandboxDialogAction::None;
        }
        match key.code {
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(SANDBOX_ADD_KINDS.len() - 1);
                SandboxDialogAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                SandboxDialogAction::None
            }
            KeyCode::Enter => SandboxDialogAction::Submit,
            KeyCode::Esc => SandboxDialogAction::Close,
            _ => SandboxDialogAction::None,
        }
    }
}

impl Default for AddSandboxDialog {
    fn default() -> Self {
        Self::new()
    }
}

// --- Resume ---

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
    /// Enter or `o` on the running flow's link: open it in the default
    /// browser.
    OpenFlowUrl,
    /// `c` on the running flow's link: copy it to the clipboard.
    CopyFlowUrl,
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
    /// The flow's one-line answer to the open and copy buttons: the
    /// last action the user took on the link. Cleared when the flow
    /// sends a fresh instruction or progress note.
    pub flow_notice: Option<String>,
    /// When the flow stage began — the clock the progress spinner
    /// animates off, and the marker for "contacting the provider"
    /// before the flow's first update lands.
    pub flow_started: Option<std::time::Instant>,
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
            flow_notice: None,
            flow_started: None,
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
            DialogStage::Flow => {
                if self.flow_url().is_some() {
                    "enter opens · c copies the link · esc cancels".to_owned()
                } else {
                    "esc cancels the login".to_owned()
                }
            }
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
            if let Some(text) = super::clipboard::read() {
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
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.provider.uses_flow())
                {
                    // A flow provider has no key to type: the stage
                    // flips and the app spawns the flow.
                    self.status = DialogStatus::Validating;
                    self.flow_instruction = None;
                    self.flow_waiting = None;
                    self.flow_started = Some(std::time::Instant::now());
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

    /// The link the running flow is waiting on, once it has reported
    /// one. The flow stage renders it as a host label and the buttons
    /// open or copy the full string.
    pub fn flow_url(&self) -> Option<&str> {
        self.flow_instruction
            .as_ref()
            .and_then(|instruction| instruction.url.as_deref())
    }

    /// Notes the outcome of the open or copy button under the flow.
    pub(crate) fn note_flow(&mut self, notice: impl Into<String>) {
        self.flow_notice = Some(notice.into());
    }

    /// Keys while a flow runs: esc cancels the dialog; enter or `o`
    /// opens the link, `c` copies it. Before the flow reports a URL
    /// the buttons have nothing to act on, so everything but esc is
    /// inert.
    fn handle_flow_key(&mut self, key: KeyEvent) -> DialogAction {
        match key.code {
            KeyCode::Esc => DialogAction::CloseCancelFlow,
            KeyCode::Enter | KeyCode::Char('o') | KeyCode::Char('O')
                if self.flow_url().is_some() =>
            {
                DialogAction::OpenFlowUrl
            }
            KeyCode::Char('c') | KeyCode::Char('C') if self.flow_url().is_some() => {
                DialogAction::CopyFlowUrl
            }
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
            LoginProvider::OpenCode => "opencode",
            LoginProvider::OpenCodeGo => "opencode-go",
            LoginProvider::Copilot => "copilot",
            LoginProvider::ChatGpt => "chatgpt",
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
            if let Some(text) = super::clipboard::read() {
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
