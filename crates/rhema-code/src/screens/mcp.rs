//! The settings MCP page: the active profile's servers as rows, the
//! validation state each carries, and the add-server dialog — the
//! state machine behind the install flow.

use super::*;

/// One row of the MCP page: one server of the active profile's map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRow {
    /// The server's name — the profile map's key.
    pub name: String,
    /// How it is reached: `stdio` for a command, `http` for a URL.
    /// `None` when the entry names neither (or both) — the state the
    /// row's notice explains.
    pub transport: Option<&'static str>,
    /// The user-written description feeding the prompt disclosure.
    pub description: String,
    /// Whether the session muted the server: the row lists dim, and
    /// the muted name rides the config's disabled set the manager
    /// reads.
    pub disabled: bool,
    /// The notice the entry earns at assembly time, in the assembly's
    /// own wording — an invalid entry stays listed so it can be fixed
    /// or removed here.
    pub notice: Option<String>,
}

/// The profile's `mcp_servers` map as rendered rows, in name order,
/// each tagged muted from `disabled`. Validation runs through
/// [`rhema_config::mcp::McpConfig::from_entries`] — the same fold the
/// assembly runs — so a row the manager would keep is clean and a row
/// it would skip carries the notice that says why.
pub fn mcp_server_rows(
    entries: &BTreeMap<String, rhema_config::mcp::McpServerEntry>,
    disabled: &BTreeSet<String>,
) -> Vec<McpServerRow> {
    let resolved = rhema_config::mcp::McpConfig::from_entries(entries);
    let kept: Vec<&str> = resolved.servers.iter().map(|s| s.name.as_str()).collect();
    entries
        .iter()
        .map(|(name, entry)| {
            let notice = if kept.contains(&name.as_str()) {
                None
            } else {
                Some(match entry.resolve(name) {
                    Err(reason) => format!("mcp server `{name}` skipped: {reason}"),
                    // Unreachable while this row and the fold run the
                    // same resolve; the gate's verdict still shows.
                    Ok(_) => format!("mcp server `{name}` skipped"),
                })
            };
            McpServerRow {
                name: name.clone(),
                transport: match (&entry.command, &entry.url) {
                    (Some(_), None) => Some("stdio"),
                    (None, Some(_)) => Some("http"),
                    _ => None,
                },
                description: entry.description.clone(),
                disabled: disabled.contains(name),
                notice,
            }
        })
        .collect()
}

/// The transports the add-server dialog offers, in dialog order: the
/// marker the row carries and the one-line gloss under it.
pub const MCP_TRANSPORTS: &[(&str, &str)] = &[
    ("stdio", "spawn a command speaking JSON-RPC over stdio"),
    ("http", "post to a Streamable HTTP endpoint"),
];

/// Which half of the add-server dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDialogStage {
    /// The name input.
    Name,
    /// The description input.
    Description,
    /// The transport list: a stdio command or an HTTP URL.
    Transport,
    /// The argv line, whitespace-split, for a stdio server.
    Command,
    /// The URL line for an HTTP server.
    Url,
    /// The editable env lines of a stdio server.
    EnvLines,
    /// The editable header lines of an HTTP server.
    HeaderLines,
    /// The optional bash-name override.
    CommandName,
}

/// What one add-server-dialog keypress means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDialogAction {
    /// Nothing to do.
    None,
    /// Enter built a valid entry: the app inserts it and persists.
    Submit,
    /// Esc left the dialog from the name stage.
    Close,
}

/// The editable `KEY=value` lines of the env and header stages: one
/// buffer, one cursor line, enter opening a line and a blank last
/// line finishing. A pasted or typed line may hold anything — the
/// shape check runs when the lines are read as pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLines {
    /// The lines, the last one usually blank.
    pub lines: Vec<String>,
    /// The line the editing keys land in.
    pub cursor: usize,
}

impl PairLines {
    /// One blank line, cursor on it.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: 0,
        }
    }

    /// The line the cursor sits on.
    pub fn current(&self) -> &str {
        self.lines
            .get(self.cursor)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Steps the cursor up, clamped at the first line.
    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Steps the cursor down, clamped at the last line.
    pub fn down(&mut self) {
        if self.cursor + 1 < self.lines.len() {
            self.cursor += 1;
        }
    }

    /// Types one character into the cursor line.
    pub fn push_char(&mut self, c: char) {
        self.lines[self.cursor].push(c);
    }

    /// Backspace: one character off the cursor line; at its start the
    /// line joins the one above, the way an editor joins.
    pub fn backspace(&mut self) {
        if self.lines[self.cursor].pop().is_none() && self.cursor > 0 {
            self.cursor -= 1;
            let tail = self.lines.remove(self.cursor + 1);
            self.lines[self.cursor].push_str(&tail);
        }
    }

    /// Enter: on the blank last line it finishes (`true`); anywhere
    /// else it opens a blank line under the cursor and moves to it.
    pub fn enter(&mut self) -> bool {
        let last = self.cursor + 1 == self.lines.len();
        if last && self.lines[self.cursor].trim().is_empty() {
            return true;
        }
        self.lines.insert(self.cursor + 1, String::new());
        self.cursor += 1;
        false
    }

    /// A bracketed paste into the cursor line: a line is a line, so
    /// control characters — newlines above all — drop.
    pub fn paste(&mut self, text: &str) {
        self.lines[self.cursor].extend(text.chars().filter(|c| !c.is_control()));
    }

    /// The lines as pairs. Every non-empty line must split at a `=`
    /// into a non-blank key and its value (the value may be empty and
    /// may itself hold `=`); a key said twice refuses the buffer
    /// rather than silently overriding.
    pub fn pairs(&self) -> Result<BTreeMap<String, String>, String> {
        let mut pairs = BTreeMap::new();
        for line in &self.lines {
            if line.trim().is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("`{line}` is not KEY=value; the '=' separates them"))?;
            let key = key.trim();
            if key.is_empty() {
                return Err(format!("`{line}` has an empty key"));
            }
            if pairs.contains_key(key) {
                return Err(format!("the key {key} appears twice"));
            }
            pairs.insert(key.to_owned(), value.to_owned());
        }
        Ok(pairs)
    }
}

impl Default for PairLines {
    fn default() -> Self {
        Self::new()
    }
}

/// The pop-in dialog over the MCP page: the name, the description, the
/// transport, the command or URL it names, the env or header lines
/// that ride it, and the optional bash-name override. Building the
/// entry is the dialog's; inserting it and persisting the profile are
/// the app's, through the same profile-write path every settings page
/// uses.
#[derive(Debug)]
pub struct AddMcpServerDialog {
    /// Which stage of the flow is showing.
    pub stage: McpDialogStage,
    /// The highlighted row of the transport list.
    pub selected: usize,
    /// The line being typed: the stage's single-line inputs.
    pub input: String,
    /// Why the last enter did not advance, when it did not: the text
    /// the status line renders in red.
    pub error: Option<String>,
    /// The accepted name, from the name stage on.
    pub name: String,
    /// The accepted description, from the description stage on.
    pub description: String,
    /// The stdio argv, whitespace-split, once the command stage took
    /// one. At most one of `command` and `url` holds a value: picking
    /// a transport clears the other's field, and the final build
    /// checks the pair again.
    pub command: Option<Vec<String>>,
    /// The HTTP endpoint URL, once the url stage took one.
    pub url: Option<String>,
    /// The stdio env lines, from the env stage on.
    pub env: PairLines,
    /// The HTTP header lines, from the headers stage on.
    pub headers: PairLines,
    /// The accepted bash-name override; empty means none.
    pub command_name: String,
    /// The server names taken when the dialog opened: the name stage
    /// refuses them. A snapshot, not a window — the dialog is modal.
    pub(crate) taken: BTreeSet<String>,
}

impl AddMcpServerDialog {
    /// A dialog at the name stage, refusing the profile's current
    /// server names.
    pub fn new(taken: BTreeSet<String>) -> Self {
        Self {
            stage: McpDialogStage::Name,
            selected: 0,
            input: String::new(),
            error: None,
            name: String::new(),
            description: String::new(),
            command: None,
            url: None,
            env: PairLines::new(),
            headers: PairLines::new(),
            command_name: String::new(),
            taken,
        }
    }

    /// The transport the highlighted row stands for, as the pair the
    /// const table spells.
    pub fn selected_transport(&self) -> (&'static str, &'static str) {
        MCP_TRANSPORTS[self.selected.min(MCP_TRANSPORTS.len() - 1)]
    }

    /// The hint under the input, per the stage.
    pub fn input_hint(&self) -> String {
        match self.stage {
            McpDialogStage::Name => {
                "lowercase letters, digits, '-' and '_' · enter continues · esc cancels".to_owned()
            }
            McpDialogStage::Description => {
                "what the server offers · enter continues · esc back".to_owned()
            }
            McpDialogStage::Transport => "arrows choose · enter continues · esc back".to_owned(),
            McpDialogStage::Command => {
                "program and arguments, split on spaces · enter continues · esc back".to_owned()
            }
            McpDialogStage::Url => "the endpoint URL · enter continues · esc back".to_owned(),
            McpDialogStage::EnvLines | McpDialogStage::HeaderLines => {
                "KEY=value per line · enter adds one, enter on a blank line continues · esc back"
                    .to_owned()
            }
            McpDialogStage::CommandName => {
                "optional bash name override · enter stores · esc back".to_owned()
            }
        }
    }

    /// The input's label, per the stage's prompt; empty on the stages
    /// that render no text line.
    pub fn input_label(&self) -> &'static str {
        match self.stage {
            McpDialogStage::Name => "name: ",
            McpDialogStage::Description => "description: ",
            McpDialogStage::Command => "command: ",
            McpDialogStage::Url => "url: ",
            McpDialogStage::CommandName => "bash name: ",
            _ => "",
        }
    }

    /// A bracketed paste landed: into the single-line input, or into
    /// the lines editor's cursor line. Control characters drop in both
    /// — a pasted newline must not fire a stage advance.
    pub fn paste(&mut self, text: &str) {
        match self.stage {
            McpDialogStage::Transport => {}
            McpDialogStage::EnvLines => self.env.paste(text),
            McpDialogStage::HeaderLines => self.headers.paste(text),
            _ => self.input.extend(text.chars().filter(|c| !c.is_control())),
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> McpDialogAction {
        // ctrl-v (ctrl-shift-v included) reads the clipboard, like the
        // other dialogs' inputs.
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = super::clipboard::read() {
                self.paste(&text);
            }
            return McpDialogAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return McpDialogAction::None;
        }
        match self.stage {
            McpDialogStage::Transport => self.handle_transport_key(key),
            McpDialogStage::EnvLines | McpDialogStage::HeaderLines => self.handle_lines_key(key),
            _ => self.handle_text_key(key),
        }
    }

    /// Keys on the transport list: arrows choose, enter descends into
    /// the command or URL input, esc returns to the description.
    /// Choosing a transport clears the other's field — one server is
    /// one transport, and a retyped choice starts fresh.
    fn handle_transport_key(&mut self, key: KeyEvent) -> McpDialogAction {
        match key.code {
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(MCP_TRANSPORTS.len() - 1);
                McpDialogAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                McpDialogAction::None
            }
            KeyCode::Enter => {
                let stdio = self.selected == 0;
                self.command = None;
                self.url = None;
                self.input.clear();
                self.error = None;
                self.stage = if stdio {
                    McpDialogStage::Command
                } else {
                    McpDialogStage::Url
                };
                McpDialogAction::None
            }
            KeyCode::Esc => {
                self.input = self.description.clone();
                self.stage = McpDialogStage::Description;
                McpDialogAction::None
            }
            _ => McpDialogAction::None,
        }
    }

    /// Keys on the env or header lines: the lines editor takes the
    /// editing keys, enter on the blank last line descends into the
    /// bash-name stage, esc returns to the transport list.
    fn handle_lines_key(&mut self, key: KeyEvent) -> McpDialogAction {
        let lines = match self.stage {
            McpDialogStage::EnvLines => &mut self.env,
            _ => &mut self.headers,
        };
        match key.code {
            KeyCode::Up => {
                lines.up();
            }
            KeyCode::Down => lines.down(),
            KeyCode::Char(c) => lines.push_char(c),
            KeyCode::Backspace => lines.backspace(),
            KeyCode::Enter => {
                if lines.enter() {
                    // The shape check runs here too: an error keeps
                    // the stage up, so the bad line is fixed before
                    // the name stage asks anything else.
                    match lines.pairs() {
                        Ok(_) => {
                            self.error = None;
                            self.input.clear();
                            self.stage = McpDialogStage::CommandName;
                        }
                        Err(err) => self.error = Some(err),
                    }
                }
            }
            KeyCode::Esc => {
                self.error = None;
                self.stage = McpDialogStage::Transport;
            }
            _ => {}
        }
        McpDialogAction::None
    }

    /// Keys on the single-line stages: typing edits, enter validates
    /// and advances, esc steps back one stage (out of the name stage
    /// is out of the dialog).
    fn handle_text_key(&mut self, key: KeyEvent) -> McpDialogAction {
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
                self.input.clear();
                self.stage = match self.stage {
                    // Out of the first stage is out of the dialog.
                    McpDialogStage::Name => return McpDialogAction::Close,
                    McpDialogStage::Description => McpDialogStage::Name,
                    McpDialogStage::Command | McpDialogStage::Url => McpDialogStage::Transport,
                    // The bash-name stage's parent is wherever the
                    // lines were edited.
                    McpDialogStage::CommandName => {
                        if self.url.is_some() {
                            McpDialogStage::HeaderLines
                        } else {
                            McpDialogStage::EnvLines
                        }
                    }
                    _ => unreachable!("the text keys own the single-line stages"),
                };
            }
            _ => {}
        }
        McpDialogAction::None
    }

    /// Enter on a single-line stage: validate the line, store it, and
    /// step forward — or name the problem and stay. The bash-name
    /// stage's enter is the dialog's store: it builds and validates
    /// the whole entry.
    fn accept_line(&mut self) -> McpDialogAction {
        let text = self.input.trim().to_owned();
        match self.stage {
            McpDialogStage::Name => {
                if text.is_empty() {
                    return McpDialogAction::None;
                }
                if !server_name_charset(&text) {
                    self.error =
                        Some("names must be lowercase letters, digits, '-' and '_'".to_owned());
                    return McpDialogAction::None;
                }
                if self.taken.contains(&text) {
                    self.error = Some(format!("a server named {text} already exists"));
                    return McpDialogAction::None;
                }
                self.name = text;
                self.input.clear();
                self.stage = McpDialogStage::Description;
            }
            McpDialogStage::Description => {
                if text.is_empty() {
                    self.error = Some("a description is required".to_owned());
                    return McpDialogAction::None;
                }
                self.description = text;
                self.input.clear();
                self.stage = McpDialogStage::Transport;
            }
            McpDialogStage::Command => {
                let argv: Vec<String> = self.input.split_whitespace().map(str::to_owned).collect();
                if argv.is_empty() {
                    self.error = Some("name the program to run".to_owned());
                    return McpDialogAction::None;
                }
                self.command = Some(argv);
                self.url = None;
                self.input.clear();
                self.env = PairLines::new();
                self.stage = McpDialogStage::EnvLines;
            }
            McpDialogStage::Url => {
                if text.is_empty() {
                    self.error = Some("the endpoint URL is required".to_owned());
                    return McpDialogAction::None;
                }
                self.url = Some(text);
                self.command = None;
                self.input.clear();
                self.headers = PairLines::new();
                self.stage = McpDialogStage::HeaderLines;
            }
            McpDialogStage::CommandName => {
                self.command_name = text;
                return self.submit();
            }
            _ => {}
        }
        McpDialogAction::None
    }

    /// Enter on the bash-name stage: build the entry and run the
    /// config layer's own validation over it. An error renders inline
    /// and keeps the dialog up; success hands the transition to the
    /// app.
    fn submit(&mut self) -> McpDialogAction {
        match self.build() {
            Ok(_) => McpDialogAction::Submit,
            Err(err) => {
                self.error = Some(err);
                McpDialogAction::None
            }
        }
    }

    /// The entry the dialog collected, with its name: the config
    /// layer's resolve is the validator — name charset, transport
    /// XOR, and the pairs' placement are its rules, so the stored
    /// entry is one the assembly will keep.
    pub fn build(&self) -> Result<(String, rhema_config::mcp::McpServerEntry), String> {
        if self.name.is_empty() {
            return Err("a name is required".to_owned());
        }
        if self.taken.contains(&self.name) {
            return Err(format!("a server named {} already exists", self.name));
        }
        let env = match self.command.is_some() {
            true => self.env.pairs()?,
            false => BTreeMap::new(),
        };
        let headers = match self.url.is_some() {
            true => self.headers.pairs()?,
            false => BTreeMap::new(),
        };
        let entry = rhema_config::mcp::McpServerEntry {
            description: self.description.clone(),
            command: self.command.clone(),
            url: self.url.clone(),
            env,
            headers,
            startup_timeout: None,
            timeout: None,
            command_name: (!self.command_name.is_empty()).then(|| self.command_name.clone()),
        };
        // The resolution result is discarded: it is the validation,
        // not the shape the dialog hands over — the stored form is
        // the entry itself, which the profile map round-trips.
        entry
            .resolve(&self.name)
            .map(|_| (self.name.clone(), entry))
    }
}

/// Whether a server name matches the charset the config layer
/// enforces. The dialog checks early for inline feedback; the entry's
/// own resolve remains the rule this only mirrors.
fn server_name_charset(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
