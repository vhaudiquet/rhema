//! The settings Decision models page: the active profile's decision
//! instances as rows, and the add-instance dialog, the state machine
//! behind the kind, name, endpoint, account, and key entry.

use super::*;

/// One kind the add dialog offers: the registry's spelling, the
/// display name the list shows, the instance name the name stage
/// suggests, and the stages the dialog walks after the kind. A custom
/// Jev endpoint is the `jev` kind with a `base_url`
/// (`specs/rhema-decision.md` §11, decision 5): the registry knows
/// three kinds, the dialog offers four rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionKind {
    /// The registry's spelling, what the file's `provider` holds.
    pub kind: &'static str,
    /// The display name, the kind list's row.
    pub display: &'static str,
    /// The instance name the name stage suggests.
    pub suggestion: &'static str,
    /// The stages after the kind stage, in order, ending in the key.
    pub stages: &'static [DecisionDialogStage],
}

impl DecisionKind {
    /// Whether this row asks for an endpoint: the custom kind.
    pub fn custom(self) -> bool {
        self.stages.contains(&DecisionDialogStage::Endpoint)
    }
}

/// The kinds the add dialog offers, in dialog order.
pub const DECISION_KINDS: &[DecisionKind] = &[
    DecisionKind {
        kind: "jev",
        display: "Jev",
        suggestion: "Jev",
        stages: &[DecisionDialogStage::Name, DecisionDialogStage::Key],
    },
    DecisionKind {
        kind: "clef",
        display: "Clef",
        suggestion: "Clef",
        stages: &[
            DecisionDialogStage::Name,
            DecisionDialogStage::Account,
            DecisionDialogStage::Key,
        ],
    },
    DecisionKind {
        kind: "openrouter",
        display: "OpenRouter",
        suggestion: "OpenRouter",
        stages: &[DecisionDialogStage::Name, DecisionDialogStage::Key],
    },
    DecisionKind {
        kind: "jev",
        display: "Custom Jev endpoint",
        suggestion: "Local Jev",
        stages: &[
            DecisionDialogStage::Name,
            DecisionDialogStage::Endpoint,
            DecisionDialogStage::Key,
        ],
    },
];

/// One row of the Decision models page: one instance of the active
/// profile's map, or the add row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRow {
    /// The instance's name, the profile map's key.
    pub name: String,
    /// Which adapter serves it, stored as given.
    pub kind: String,
    /// The model the instance asks: its own when it names one, else
    /// the kind's default, what `decide` sends for a blank.
    pub model: String,
    /// The endpoint, when the instance names its own.
    pub base_url: Option<String>,
    /// Whether a key resolves for the instance: the environment's,
    /// the secret store's, or the file's.
    pub keyed: bool,
    /// Whether `decide` asks this instance by default.
    pub default: bool,
    /// Whether this is the add-an-instance row.
    pub add: bool,
}

/// The profile's decision instances as rendered rows, in name order,
/// then the add row. The default is [`rhema_config::DecisionSettings::active_instance`]'s:
/// the one `instance` names, else the first by name.
pub fn decision_rows(
    settings: Option<&rhema_config::DecisionSettings>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> Vec<DecisionRow> {
    let default = settings
        .and_then(rhema_config::DecisionSettings::active_instance)
        .map(|(name, _)| name);
    let mut rows: Vec<DecisionRow> = settings
        .map(|settings| {
            settings
                .instances
                .iter()
                .map(|(name, instance)| DecisionRow {
                    name: name.clone(),
                    kind: instance.provider.clone(),
                    model: instance
                        .model
                        .as_deref()
                        .map(str::trim)
                        .filter(|model| !model.is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            rhema_decision_registry::default_model(&instance.provider)
                                .map(str::to_owned)
                        })
                        .unwrap_or_default(),
                    base_url: instance
                        .base_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned),
                    keyed: instance.api_key(secrets).is_some(),
                    default: default == Some(name.as_str()),
                    add: false,
                })
                .collect()
        })
        .unwrap_or_default();
    rows.push(DecisionRow {
        name: String::new(),
        kind: String::new(),
        model: String::new(),
        base_url: None,
        keyed: false,
        default: false,
        add: true,
    });
    rows
}

/// Which stage of the add dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionDialogStage {
    /// The kind list.
    Kind,
    /// The name input.
    Name,
    /// The endpoint input, the custom kind only.
    Endpoint,
    /// The Cloudflare account id input, Clef only.
    Account,
    /// The key input, every kind's last stage.
    Key,
}

impl DecisionDialogStage {
    /// The input's label; empty on the kind stage, which renders no
    /// text line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name: ",
            Self::Endpoint => "endpoint: ",
            Self::Account => "account id: ",
            Self::Key => "api key: ",
            Self::Kind => "",
        }
    }

    /// The hint under the input, per the stage and the chosen kind.
    /// Each fits the dialog's seventy-cell status line.
    pub fn hint(self, kind: DecisionKind) -> &'static str {
        match self {
            Self::Kind => "arrows choose · enter continues · esc cancels",
            Self::Name => "name this instance · enter continues · esc back",
            Self::Endpoint => "the server's base URL, http://localhost:30000 · enter · esc back",
            Self::Account => "the Cloudflare account id, 32 hex characters · enter · esc back",
            Self::Key => match kind.display {
                "Clef" => "Cloudflare mints the token · ctrl-v paste · enter stores · esc back",
                "OpenRouter" => "openrouter.ai mints it · ctrl-v paste · enter stores · esc back",
                "Custom Jev endpoint" => {
                    "its key, or anything locally · ctrl-v paste · enter stores · esc back"
                }
                _ => "typesafe.ai mints it · ctrl-v paste · enter stores · esc back",
            },
        }
    }
}

/// What one add-dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionDialogAction {
    /// Nothing to do.
    None,
    /// Enter built a valid instance: the app stores it, places the
    /// key, and persists.
    Submit,
    /// Esc left the dialog from the kind stage.
    Close,
}

/// What the dialog built: the instance's fields and the key beside
/// them, the app places the key and stores the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionBuilt {
    /// The instance's name.
    pub name: String,
    /// The registry's spelling.
    pub kind: &'static str,
    /// The key as entered.
    pub key: String,
    /// The endpoint, the custom kind only.
    pub base_url: Option<String>,
    /// The Cloudflare account id, Clef only.
    pub account_id: Option<String>,
}

/// The pop-in dialog over the Decision models page: pick the kind,
/// name the instance, answer the kind's own stages (an endpoint, an
/// account id), enter the key. The key lands in the secret store when the
/// store accepts it, the app's `submit_decision_provider` decides;
/// the dialog only carries the text.
#[derive(Debug)]
pub struct AddDecisionDialog {
    /// Which stage of the flow is showing.
    pub stage: DecisionDialogStage,
    /// The highlighted row of the kind list.
    pub selected: usize,
    /// The line being typed: the stage's single-line inputs.
    pub input: String,
    /// Why the last enter did not advance, when it did not.
    pub error: Option<String>,
    /// The accepted kind, from the kind stage on.
    pub choice: Option<DecisionKind>,
    /// The accepted name, from the name stage on.
    pub name: String,
    /// The accepted endpoint, from the endpoint stage on.
    pub base_url: Option<String>,
    /// The accepted account id, from the account stage on.
    pub account_id: Option<String>,
    /// The accepted key. Held in the dialog until Submit, the input
    /// line clears under it.
    pub key: String,
    /// The instance names taken when the dialog opened: the name
    /// stage refuses them. A snapshot, not a window, the dialog is
    /// modal.
    taken: BTreeSet<String>,
}

impl AddDecisionDialog {
    /// A dialog at the kind stage, refusing the profile's current
    /// instance names.
    pub fn new(taken: BTreeSet<String>) -> Self {
        Self {
            stage: DecisionDialogStage::Kind,
            selected: 0,
            input: String::new(),
            error: None,
            choice: None,
            name: String::new(),
            base_url: None,
            account_id: None,
            key: String::new(),
            taken,
        }
    }

    /// The kind the highlighted row stands for.
    pub fn selected_kind(&self) -> DecisionKind {
        DECISION_KINDS[self.selected.min(DECISION_KINDS.len() - 1)]
    }

    /// The stages the chosen kind walks after the kind stage; none
    /// before a kind is chosen.
    fn stages(&self) -> &'static [DecisionDialogStage] {
        self.choice.map_or(&[], |choice| choice.stages)
    }

    /// Where the showing stage sits in the kind's list.
    fn stage_index(&self) -> usize {
        self.stages()
            .iter()
            .position(|stage| *stage == self.stage)
            .unwrap_or(0)
    }

    /// The hint under the input, per the stage.
    pub fn input_hint(&self) -> String {
        self.stage
            .hint(self.choice.unwrap_or_else(|| self.selected_kind()))
            .to_owned()
    }

    /// The input's label, per the stage's prompt; empty on the kind
    /// stage, which renders no text line.
    pub fn input_label(&self) -> &'static str {
        self.stage.label()
    }

    /// Shows a stage, its input carrying what the stage last
    /// accepted: the key line opens on the custom kind's `local`, a
    /// server without auth takes any text.
    fn enter_stage(&mut self, stage: DecisionDialogStage) {
        self.stage = stage;
        self.error = None;
        self.input = match stage {
            DecisionDialogStage::Name => self.name.clone(),
            DecisionDialogStage::Endpoint => self.base_url.clone().unwrap_or_default(),
            DecisionDialogStage::Account => self.account_id.clone().unwrap_or_default(),
            DecisionDialogStage::Key => {
                if !self.key.is_empty() {
                    self.key.clone()
                } else if self.choice.is_some_and(DecisionKind::custom) {
                    "local".to_owned()
                } else {
                    String::new()
                }
            }
            DecisionDialogStage::Kind => String::new(),
        };
    }

    /// A bracketed paste landed: into the single-line input. Control
    /// characters drop, a pasted newline must not fire a stage
    /// advance.
    pub fn paste(&mut self, text: &str) {
        if self.stage != DecisionDialogStage::Kind {
            self.input.extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> DecisionDialogAction {
        // ctrl-v (ctrl-shift-v included) reads the clipboard, like the
        // other dialogs' inputs.
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = super::clipboard::read() {
                self.paste(&text);
            }
            return DecisionDialogAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return DecisionDialogAction::None;
        }
        match self.stage {
            DecisionDialogStage::Kind => self.handle_kind_key(key),
            _ => self.handle_text_key(key),
        }
    }

    /// Keys on the kind list: arrows choose, enter descends into the
    /// name input carrying the kind's suggested name, esc closes.
    fn handle_kind_key(&mut self, key: KeyEvent) -> DecisionDialogAction {
        match key.code {
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(DECISION_KINDS.len() - 1);
                DecisionDialogAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                DecisionDialogAction::None
            }
            KeyCode::Enter => {
                let choice = self.selected_kind();
                self.choice = Some(choice);
                self.error = None;
                // The suggested name is the kind's own, numbered when
                // taken: Jev, "Jev 2", the Providers page's convention
                // for a first and second instance of a vendor.
                let base = choice.suggestion;
                self.name = if !self.taken.contains(base) {
                    base.to_owned()
                } else {
                    (2..)
                        .map(|n| format!("{base} {n}"))
                        .find(|name| !self.taken.contains(name))
                        .unwrap_or_else(|| format!("{base} {}", self.taken.len() + 1))
                };
                self.enter_stage(DecisionDialogStage::Name);
                DecisionDialogAction::None
            }
            KeyCode::Esc => DecisionDialogAction::Close,
            _ => DecisionDialogAction::None,
        }
    }

    /// Keys on the text lines: typing lands in the input, enter
    /// accepts, esc steps back one stage, out of the name stage is
    /// out of the dialog to the kind list.
    fn handle_text_key(&mut self, key: KeyEvent) -> DecisionDialogAction {
        match key.code {
            KeyCode::Char(c) => {
                self.input.push(c);
                self.error = None;
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.error = None;
            }
            KeyCode::Enter => return self.accept_line(),
            KeyCode::Esc => {
                let previous = self.stage_index().checked_sub(1);
                match previous {
                    Some(index) => {
                        let stage = self.stages()[index];
                        self.enter_stage(stage);
                    }
                    None => {
                        self.input.clear();
                        self.error = None;
                        self.choice = None;
                        self.stage = DecisionDialogStage::Kind;
                    }
                }
            }
            _ => {}
        }
        DecisionDialogAction::None
    }

    /// Enter on a text stage: the stage's validation, then the next
    /// stage, or Submit from the key.
    fn accept_line(&mut self) -> DecisionDialogAction {
        let text = self.input.trim().to_owned();
        match self.stage {
            DecisionDialogStage::Name => {
                if text.is_empty() {
                    return DecisionDialogAction::None;
                }
                // Instance names are freeform, the Providers page's
                // rule: any non-empty trimmed line that does not
                // collide. The map key is the name, nothing
                // agent-facing reads it.
                if self.taken.contains(&text) {
                    self.error = Some(format!("An instance named {text} already exists"));
                    return DecisionDialogAction::None;
                }
                self.name = text;
            }
            DecisionDialogStage::Endpoint => {
                if !(text.starts_with("http://") || text.starts_with("https://")) {
                    self.error = Some("The endpoint is a URL, http:// or https://".to_owned());
                    return DecisionDialogAction::None;
                }
                self.base_url = Some(text);
            }
            DecisionDialogStage::Account => {
                if text.is_empty() {
                    self.error = Some(
                        "An account id is required (or export CLOUDFLARE_ACCOUNT_ID)".to_owned(),
                    );
                    return DecisionDialogAction::None;
                }
                self.account_id = Some(text);
            }
            DecisionDialogStage::Key => {
                if text.is_empty() {
                    self.error = Some(
                        "A key is required (a local server ignores it, any text serves)".to_owned(),
                    );
                    return DecisionDialogAction::None;
                }
                self.key = text;
                self.input.clear();
                return DecisionDialogAction::Submit;
            }
            DecisionDialogStage::Kind => {}
        }
        let next = self.stages()[self.stage_index() + 1];
        self.enter_stage(next);
        DecisionDialogAction::None
    }

    /// The accepted instance: the name, the kind, the key, and the
    /// kind's own fields. The app calls this on the Submit action.
    pub fn build(&self) -> Result<DecisionBuilt, String> {
        let Some(choice) = self.choice else {
            return Err("A kind is required".to_owned());
        };
        if self.name.is_empty() {
            return Err("A name is required".to_owned());
        }
        if self.taken.contains(&self.name) {
            return Err(format!("An instance named {} already exists", self.name));
        }
        Ok(DecisionBuilt {
            name: self.name.clone(),
            kind: choice.kind,
            key: self.key.clone(),
            base_url: self.base_url.clone(),
            account_id: self.account_id.clone(),
        })
    }
}
