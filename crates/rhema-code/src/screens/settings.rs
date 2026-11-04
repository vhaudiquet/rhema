//! The settings screen: its sections, the rows each page renders, and the screen state itself.

use super::*;

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
    /// The active profile's MCP servers: list, add, mute, remove.
    Mcp,
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
        SettingsSection::Mcp,
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
            SettingsSection::Mcp => "MCP",
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
    /// Enter on the MCP page's add row: the add-server dialog opens.
    OpenMcpDialog,
    /// The add-server dialog built a valid entry: insert it and
    /// persist.
    SubmitMcpDialog,
    /// Esc left the add-server dialog.
    CloseMcpDialog,
    /// Enter or Space on an MCP server row: flip the server's muted
    /// state.
    ToggleMcpServer,
    /// `y` on the remove-server confirm: the entry goes from the
    /// profile.
    RemoveMcpServer,
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

/// Where a skill lives: shipped with rhema, in the config directory,
/// in the workspace, or in the cross-agent `~/.agents/skills` — the
/// same shadowing order discovery reads, workspace highest and
/// `.agents` lowest, so a row never names a hidden copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Seeded by rhema from the binary's bundled set.
    Bundled,
    /// A folder under the config directory.
    Global,
    /// A folder under the workspace's `.rhema/skills`.
    Workspace,
    /// A folder under the cross-agent `~/.agents/skills`.
    Agents,
}

impl SkillSource {
    /// The tag the page draws after the name.
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::Bundled => "bundled",
            SkillSource::Global => "global",
            SkillSource::Workspace => "workspace",
            SkillSource::Agents => "agents",
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
    // The MCP page's snapshot.
    /// The active profile's servers, muted flags included, synced
    /// from the config whenever the app mutates it.
    pub mcp: Vec<McpServerRow>,
    /// The add-server dialog, when it is up.
    pub mcp_dialog: Option<AddMcpServerDialog>,
    /// The server name awaiting its removal confirmation — the `d`
    /// key arms it, `y` confirms, `n` and esc leave the row alone.
    pub mcp_confirm: Option<String>,
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
            mcp: Vec::new(),
            mcp_dialog: None,
            mcp_confirm: None,
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
                            (None, true) => "named after this workspace · vm".to_owned(),
                            (None, _) => match instance.kind.as_str() {
                                // The ready kinds derive the concrete
                                // name per workspace at assembly time.
                                "docker" | "lxd" => "named after this workspace".to_owned(),
                                _ => "no target set".to_owned(),
                            },
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

    /// The highlighted MCP server row's name; `None` on the add row,
    /// with the dialog or the removal confirm up, or anywhere but the
    /// MCP page.
    pub fn selected_mcp_server(&self) -> Option<&str> {
        if self.section != SettingsSection::Mcp
            || self.mcp_dialog.is_some()
            || self.mcp_confirm.is_some()
        {
            return None;
        }
        self.mcp.get(self.row).map(|row| row.name.as_str())
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
            SettingsSection::Mcp => self.mcp.len() + 1,
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
            SettingsSection::Mcp => match self.selected_mcp_server() {
                Some(_) => SettingsAction::ToggleMcpServer,
                None => SettingsAction::OpenMcpDialog,
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
        if let Some(action) = self
            .mcp_dialog
            .as_mut()
            .map(|dialog| dialog.handle_key(key))
        {
            if action == McpDialogAction::Close {
                self.mcp_dialog = None;
            }
            return match action {
                McpDialogAction::None => SettingsAction::None,
                McpDialogAction::Submit => SettingsAction::SubmitMcpDialog,
                McpDialogAction::Close => SettingsAction::CloseMcpDialog,
            };
        }
        if self.mcp_confirm.is_some() {
            // The removal confirm takes every key while it is up: esc
            // must stay here, not back out of the screen behind it.
            return match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.mcp_confirm = None;
                    SettingsAction::RemoveMcpServer
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.mcp_confirm = None;
                    SettingsAction::None
                }
                _ => SettingsAction::None,
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
