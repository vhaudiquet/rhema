//! The settings web search page: the active profile's providers as
//! rows, in the chain order the `search` macro walks, and the
//! add-provider dialog, the state machine behind the key entry.

use super::*;

/// One row of the web search page: one provider of the active
/// profile's map, or the add row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchRow {
    /// The instance's name, the profile map's key.
    pub name: String,
    /// Which adapter serves it: `brave`, `exa`, or `tavily`, stored
    /// as given.
    pub kind: String,
    /// Whether the instance sits in the chain order: an unordered one
    /// lists dim and inert, `search` never reaches it.
    pub ordered: bool,
    /// The instance's position in the chain, one-based, when ordered.
    pub position: Option<usize>,
    /// Whether this is the add-a-provider row.
    pub add: bool,
}

/// The profile's web search map as rendered rows: the chain order
/// first (the order list is the display order, entries naming no
/// instance skipped the way the chain skips them), then the unordered
/// instances in name order, then the add row.
pub fn websearch_rows(
    instances: &BTreeMap<String, rhema_config::WebSearchInstance>,
    order: &[String],
) -> Vec<WebSearchRow> {
    let mut rows: Vec<WebSearchRow> = order
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let instance = instances.get(name)?;
            Some(WebSearchRow {
                name: name.clone(),
                kind: instance.provider.clone(),
                ordered: true,
                position: Some(index + 1),
                add: false,
            })
        })
        .collect();
    for (name, instance) in instances {
        if order.contains(name) {
            continue;
        }
        rows.push(WebSearchRow {
            name: name.clone(),
            kind: instance.provider.clone(),
            ordered: false,
            position: None,
            add: false,
        });
    }
    rows.push(WebSearchRow {
        name: String::new(),
        kind: String::new(),
        ordered: false,
        position: None,
        add: true,
    });
    rows
}

/// The kinds the add-provider dialog offers, in dialog order: the
/// registry's spelling and the display name the name stage suggests.
/// A plain list, the Providers page's shape — a vendor is a row, a
/// row can be picked again and again, the name stage numbers the
/// suggestions, and nothing here bounds how many instances a profile
/// holds.
pub const WEBSEARCH_KINDS: &[(&str, &str)] = &[
    ("brave", "Brave"),
    ("exa", "Exa"),
    ("tavily", "Tavily"),
    ("opencode", "OpenCode"),
];

/// Which half of the add-provider dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchDialogStage {
    /// The kind list: brave, exa, tavily, opencode.
    Kind,
    /// The name input, suggested from the kind.
    Name,
    /// The key input.
    Key,
}

/// What one add-provider-dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchDialogAction {
    /// Nothing to do.
    None,
    /// Enter built a valid provider: the app stores it, places the
    /// key, and persists.
    Submit,
    /// Esc left the dialog from the kind stage.
    Close,
}

/// The pop-in dialog over the web search page: pick the kind, name
/// the instance, enter the key. The key lands in the secret store when
/// the store accepts it, the app's `place_websearch_key` decides, the
/// dialog only carries the text.
#[derive(Debug)]
pub struct AddWebSearchDialog {
    /// Which stage of the flow is showing.
    pub stage: WebSearchDialogStage,
    /// The highlighted row of the kind list.
    pub selected: usize,
    /// The line being typed: the stage's single-line inputs.
    pub input: String,
    /// Why the last enter did not advance, when it did not: the text
    /// the status line renders in red.
    pub error: Option<String>,
    /// The accepted kind, from the kind stage on.
    pub kind: &'static str,
    /// The accepted name, from the name stage on.
    pub name: String,
    /// The accepted key, from the key stage on. Held in the dialog
    /// until Submit, the input line clears under it.
    pub key: String,
    /// The instance names taken when the dialog opened: the name
    /// stage refuses them. A snapshot, not a window, the dialog is
    /// modal.
    taken: BTreeSet<String>,
}

impl AddWebSearchDialog {
    /// A dialog at the kind stage, refusing the profile's current
    /// instance names.
    pub fn new(taken: BTreeSet<String>) -> Self {
        Self {
            stage: WebSearchDialogStage::Kind,
            selected: 0,
            input: String::new(),
            error: None,
            kind: "",
            name: String::new(),
            key: String::new(),
            taken,
        }
    }

    /// The kind the highlighted row stands for.
    pub fn selected_kind(&self) -> &'static str {
        WEBSEARCH_KINDS[self.selected.min(WEBSEARCH_KINDS.len() - 1)].0
    }

    /// The display name the highlighted row suggests for the instance:
    /// the vendor's own spelling, capitals included.
    pub fn selected_display(&self) -> &'static str {
        WEBSEARCH_KINDS[self.selected.min(WEBSEARCH_KINDS.len() - 1)].1
    }

    /// The hint under the input, per the stage.
    pub fn input_hint(&self) -> String {
        match self.stage {
            WebSearchDialogStage::Kind => {
                "arrows choose · enter continues · esc cancels".to_owned()
            }
            WebSearchDialogStage::Name => {
                "name this provider · enter continues · esc back".to_owned()
            }
            WebSearchDialogStage::Key => {
                "the vendor's dashboard mints it · ctrl-v paste · enter stores · esc back"
                    .to_owned()
            }
        }
    }

    /// The input's label, per the stage's prompt; empty on the kind
    /// stage, which renders no text line.
    pub fn input_label(&self) -> &'static str {
        match self.stage {
            WebSearchDialogStage::Name => "name: ",
            WebSearchDialogStage::Key => "api key: ",
            WebSearchDialogStage::Kind => "",
        }
    }

    /// A bracketed paste landed: into the single-line input. Control
    /// characters drop, a pasted newline must not fire a stage
    /// advance.
    pub fn paste(&mut self, text: &str) {
        if self.stage != WebSearchDialogStage::Kind {
            self.input.extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> WebSearchDialogAction {
        // ctrl-v (ctrl-shift-v included) reads the clipboard, like the
        // other dialogs' inputs.
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = super::clipboard::read() {
                self.paste(&text);
            }
            return WebSearchDialogAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return WebSearchDialogAction::None;
        }
        match self.stage {
            WebSearchDialogStage::Kind => self.handle_kind_key(key),
            _ => self.handle_text_key(key),
        }
    }

    /// Keys on the kind list: arrows choose, enter descends into the
    /// name input carrying the kind's suggested name, esc closes.
    fn handle_kind_key(&mut self, key: KeyEvent) -> WebSearchDialogAction {
        match key.code {
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(WEBSEARCH_KINDS.len() - 1);
                WebSearchDialogAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                WebSearchDialogAction::None
            }
            KeyCode::Enter => {
                self.kind = self.selected_kind();
                self.error = None;
                // The suggested name is the vendor's display name,
                // numbered when taken: Exa, "Exa 2", the providers
                // page's own convention for a first and second
                // instance of a vendor.
                let base = self.selected_display();
                if !self.taken.contains(base) {
                    self.input = base.to_owned();
                } else {
                    self.input = (2..)
                        .map(|n| format!("{base} {n}"))
                        .find(|name| !self.taken.contains(name))
                        .unwrap_or_else(|| format!("{base} {}", self.taken.len() + 1));
                }
                self.stage = WebSearchDialogStage::Name;
                WebSearchDialogAction::None
            }
            KeyCode::Esc => WebSearchDialogAction::Close,
            _ => WebSearchDialogAction::None,
        }
    }

    /// Keys on the name and key lines: typing lands in the input,
    /// enter accepts, esc steps back to the parent stage, out of the
    /// name stage's parent (the kind list) is out of the dialog.
    fn handle_text_key(&mut self, key: KeyEvent) -> WebSearchDialogAction {
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
                self.error = None;
                match self.stage {
                    WebSearchDialogStage::Name => {
                        self.input.clear();
                        self.stage = WebSearchDialogStage::Kind;
                    }
                    WebSearchDialogStage::Key => {
                        self.input = self.name.clone();
                        self.stage = WebSearchDialogStage::Name;
                    }
                    WebSearchDialogStage::Kind => {}
                }
            }
            _ => {}
        }
        WebSearchDialogAction::None
    }

    /// Enter on a text stage: the stage's validation, then advance.
    fn accept_line(&mut self) -> WebSearchDialogAction {
        let text = self.input.trim().to_owned();
        match self.stage {
            WebSearchDialogStage::Name => {
                if text.is_empty() {
                    return WebSearchDialogAction::None;
                }
                // Instance names are freeform, the Providers page's
                // rule: any non-empty trimmed line that does not
                // collide, "Exa" and "work exa" alike. The map key is
                // the name, nothing agent-facing reads it.
                if self.taken.contains(&text) {
                    self.error = Some(format!("A provider named {text} already exists"));
                    return WebSearchDialogAction::None;
                }
                self.name = text;
                self.input.clear();
                self.error = None;
                self.stage = WebSearchDialogStage::Key;
            }
            WebSearchDialogStage::Key => {
                if text.is_empty() {
                    self.error = Some(
                        "A key is required (the environment can serve it, leave \
                                       the dialog and edit config.yaml to go env-only)"
                            .to_owned(),
                    );
                    return WebSearchDialogAction::None;
                }
                self.error = None;
                self.key = text;
                self.input.clear();
                return WebSearchDialogAction::Submit;
            }
            WebSearchDialogStage::Kind => {}
        }
        WebSearchDialogAction::None
    }

    /// The accepted provider: the name, the kind, the key. The app
    /// calls this on the Submit action.
    pub fn build(&self) -> Result<(String, &'static str, String), String> {
        if self.name.is_empty() {
            return Err("A name is required".to_owned());
        }
        if self.taken.contains(&self.name) {
            return Err(format!("A provider named {} already exists", self.name));
        }
        Ok((self.name.clone(), self.kind, self.key.clone()))
    }
}
