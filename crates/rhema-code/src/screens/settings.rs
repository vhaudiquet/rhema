//! The settings screen: its sections, the rows each page renders, and the screen state itself.

use super::*;

/// A section of the settings screen, in sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    /// The profile's provider, model, and logged-in instances, the
    /// login flow lives here now.
    Providers,
    /// The models the profile can run, the catalog as a page: one row
    /// per entry, the page's own cursor walking it, and the pick under
    /// that cursor becoming the profile's model.
    Models,
    /// The named profiles: add, remove, switch, and their name colors.
    Profiles,
    /// Where `bash` runs: the sandbox kinds, their instances, the
    /// active one, and the add flow.
    Sandbox,
    /// The theme and the footer's icon flavor.
    Appearance,
    /// The discovered skills.
    Skills,
    /// The discovered subagent definitions, editable in `$EDITOR`.
    Subagents,
    /// The active profile's MCP servers: list, add, mute, remove.
    Mcp,
    /// The active profile's web search providers: list, add, remove,
    /// and the fallback order between them.
    WebSearch,
    /// The active profile's decision instances: list, add, remove,
    /// and the one `decide` asks by default.
    Decision,
    /// The allowance, the session's cost, the last turn's tokens.
    Usage,
    /// This build beside the channel's latest release, and the
    /// installer run behind the page's two rows.
    Update,
    /// Version, repository, paths.
    About,
}

impl SettingsSection {
    /// Every section, in sidebar order.
    pub const ALL: &'static [SettingsSection] = &[
        SettingsSection::Providers,
        SettingsSection::Models,
        SettingsSection::Profiles,
        SettingsSection::Sandbox,
        SettingsSection::Appearance,
        SettingsSection::Skills,
        SettingsSection::Subagents,
        SettingsSection::Mcp,
        SettingsSection::WebSearch,
        SettingsSection::Decision,
        SettingsSection::Usage,
        SettingsSection::Update,
        SettingsSection::About,
    ];

    /// The label the sidebar draws for this section.
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Providers => "Providers",
            SettingsSection::Models => "Models",
            SettingsSection::Profiles => "Profiles",
            SettingsSection::Sandbox => "Sandbox",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Skills => "Skills",
            SettingsSection::Subagents => "Subagents",
            SettingsSection::Mcp => "MCP",
            SettingsSection::WebSearch => "Web search",
            SettingsSection::Decision => "Decision models",
            SettingsSection::Usage => "Usage",
            SettingsSection::Update => "Update",
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
    /// Enter or `o` on the running flow's link: open it in the default
    /// browser.
    OpenFlowUrl,
    /// `c` on the running flow's link: copy it to the clipboard.
    CopyFlowUrl,
    /// The dialog closed (esc at any stage); a running flow task dies
    /// with it.
    CloseDialog,
    /// Enter on a Models row: the pick becomes the profile's model.
    ChooseModel,
    /// `r` after a failed catalog fetch: ask again.
    RetryModels,
    /// Cycle to the next color palette.
    CyclePalette,
    /// Toggle the theme.
    ToggleTheme,
    /// Toggle the background source: the palette's fills or the
    /// terminal's own background.
    ToggleBackground,
    /// Toggle the icon flavor.
    ToggleIcons,
    /// Cycle the input caret's shape: the terminal's own, a block, a
    /// line, an underscore.
    CycleCursor,
    /// Toggle the transcript's scrollbar.
    ToggleScrollbar,
    /// Enter or `e` on a subagent row: open the definition in `$EDITOR`.
    /// The built-in has no file, the app copies it into the global
    /// directory first, where it shadows the built-in from then on.
    EditSubagent,
    /// Space on a subagent row: flip the definition's on/off state
    /// (enter stays the edit, the skills page's toggle's twin).
    ToggleSubagent,
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
    /// Enter on the web search page's add row: the add-provider
    /// dialog opens.
    OpenWebSearchDialog,
    /// The add-provider dialog accepted kind, name, and key: store
    /// the instance, place the key, persist.
    SubmitWebSearchDialog,
    /// Esc left the add-provider dialog.
    CloseWebSearchDialog,
    /// `y` on the remove-provider confirm: the instance leaves the
    /// profile's map and its order entry goes with it.
    RemoveWebSearchProvider,
    /// `[` on a provider row: one step up the chain, or into the
    /// chain when the row was inert.
    MoveWebSearchUp,
    /// `]` on a provider row: one step down the chain, never out of
    /// it.
    MoveWebSearchDown,
    /// Enter on the Decision models page's add row: the
    /// add-instance dialog opens.
    OpenDecisionDialog,
    /// The add-instance dialog accepted its stages: store the
    /// instance, place the key, persist.
    SubmitDecisionDialog,
    /// Esc left the add-instance dialog.
    CloseDecisionDialog,
    /// `y` on the remove-instance confirm: the instance leaves the
    /// profile's map, and the default with it when it was the one.
    RemoveDecisionProvider,
    /// Enter on an instance row: `decide` asks it by default from the
    /// next call on.
    SetDefaultDecision,
    /// Enter on the Update page's check row: the manifest fetch starts.
    CheckForUpdates,
    /// Enter on the Update page's install row: the install script
    /// runs. The page only raises the action when a check found a
    /// newer release and no install is already out.
    InstallUpdate,
}

/// The Update page's snapshot: what the last version check and the
/// last install run reported. The app holds the truth, the states
/// survive closing the settings, and the opener and every result
/// sync it in here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatePage {
    /// The version check's state.
    pub check: UpdateCheck,
    /// The install run's state.
    pub install: UpdateInstall,
}

impl UpdatePage {
    /// Whether the channel carries a release above this build: the
    /// install row's one enabled state. A check that has not run, is
    /// running, or failed says no, as does a channel behind this build.
    pub fn update_available(&self) -> bool {
        matches!(
            &self.check,
            UpdateCheck::Done(Ok(latest))
                if crate::update::is_newer(latest, crate::update::current_version())
        )
    }
}

/// The version check's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateCheck {
    /// No check has run in this session yet.
    #[default]
    Idle,
    /// The manifest fetch is in flight.
    Checking,
    /// The fetch answered: the channel's latest version, or why it
    /// could not be read.
    Done(Result<String, String>),
}

/// The install run's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateInstall {
    /// No install has run in this session yet.
    #[default]
    Idle,
    /// The install script is out; its output grows unseen until the
    /// result event lands.
    Running,
    /// The run finished: the script's output tail, or the failure.
    Done(Result<String, String>),
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
    /// The frontmatter license, when the skill declares one, shown
    /// under the description, dimmer than it.
    pub license: Option<String>,
    /// Where the skill lives.
    pub source: SkillSource,
    /// Whether the skill's trigger reaches the system prompt, the
    /// config's `skills_disabled` set, inverted. Off rows still list,
    /// dimmed; only the prompt filters them out.
    pub enabled: bool,
}

/// Where a skill lives: shipped with rhema, in the config directory,
/// in the workspace, or in a cross-agent `.agents/skills` (home or
/// workspace). The row's source is the tier discovery stamped on the
/// skill, so the page never re-derives a tier from paths, and
/// discovery's shadowing by name keeps a row from naming a hidden
/// copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Seeded by rhema from the binary's bundled set.
    Bundled,
    /// A folder under the config directory.
    Global,
    /// A folder under the workspace's `.rhema/skills`.
    Workspace,
    /// A folder under a cross-agent `.agents/skills`, in the home or
    /// in the workspace.
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
/// global, and one definition that is not on disk.
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
    /// Whether the definition reaches the subagent wire's catalog,
    /// the config's `disabled_subagents` set, inverted. Off rows still
    /// list, dimmed; only discovery filters them.
    pub enabled: bool,
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
/// section's page on the right, and, over the Providers page, the
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
    /// The Models page's cursor lives in its picker instead; see
    /// [`Self::select_row`].
    pub row: usize,
    /// Appearance: the color palette on screen.
    pub colors: crate::palette::Palette,
    /// Appearance: whether the dark theme is the one on screen.
    pub theme_dark: bool,
    /// Appearance: whether the icons are the Nerd Font set.
    pub icons_nerd: bool,
    /// Appearance: whether the terminal's own background is the one on
    /// screen, no painted page fills.
    pub background_native: bool,
    /// Appearance: the caret shape the input asks the terminal to draw.
    pub cursor: crate::cursor::CursorStyle,
    /// Appearance: whether the transcript draws its scrollbar.
    pub scrollbar: bool,
    // The Providers page, live.
    /// The profile this screen edits: the session's.
    pub profile: String,
    /// The profile's stored settings, synced from the config whenever
    /// the app mutates it.
    pub stored: rhema_config::Profile,
    /// The add-instance dialog, when it is up.
    pub dialog: Option<AddInstanceDialog>,
    /// The shared model picker, the Models page's list: the same widget
    /// `/model` opens, and the wizard's model step.
    pub model_picker: ModelPicker,
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
    /// The server name awaiting its removal confirmation, the `d`
    /// key arms it, `y` confirms, `n` and esc leave the row alone.
    pub mcp_confirm: Option<String>,
    // The Web search page's snapshot.
    /// The active profile's providers, in chain order first, synced
    /// from the config whenever the app mutates it.
    pub websearch: Vec<WebSearchRow>,
    /// The add-provider dialog, when it is up.
    pub websearch_dialog: Option<AddWebSearchDialog>,
    /// The provider name awaiting its removal confirmation, the `d`
    /// key arms it, `y` confirms, `n` and esc leave the row alone.
    pub websearch_confirm: Option<String>,
    // The Decision models page's snapshot.
    /// The active profile's decision instances in name order, synced
    /// from the config whenever the app mutates it.
    pub decision: Vec<DecisionRow>,
    /// The add-instance dialog, when it is up.
    pub decision_dialog: Option<AddDecisionDialog>,
    /// The instance name awaiting its removal confirmation, the `d`
    /// key arms it, `y` confirms, `n` and esc leave the row alone.
    pub decision_confirm: Option<String>,

    // The Providers page's removal confirm.
    /// The instance name awaiting its removal confirmation, the `d`
    /// key arms it, `y` confirms, `n` and esc leave the row alone.
    pub instance_confirm: Option<String>,
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
    // The Update page's snapshot.
    /// The last check's and the last install's states, synced from the
    /// app whenever either moves.
    pub update: UpdatePage,
    // The About page's snapshot.
    /// The config file's path, when one resolves.
    pub config_path: Option<String>,
    /// The workspace root.
    pub workspace: String,
    /// How many profiles the config holds.
    pub profile_count: usize,
}

impl Default for SettingsScreen {
    /// A settings screen on the Providers page: dark theme, nerd
    /// icons, native background, empty snapshots, the opener fills
    /// the rest.
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsScreen {
    /// A settings screen on the Providers page: dark theme, plain
    /// icons, empty snapshots, the opener fills the rest.
    pub fn new() -> Self {
        Self {
            section: SettingsSection::Providers,
            row: 0,
            colors: crate::palette::Palette::Rhema,
            theme_dark: true,
            icons_nerd: true,
            background_native: true,
            cursor: crate::cursor::CursorStyle::Default,
            scrollbar: true,
            profile: "personal".to_owned(),
            stored: rhema_config::Profile::default(),
            dialog: None,
            model_picker: ModelPicker::new(Vec::new()),
            sandbox_offered: crate::sandbox::SandboxAvailability::default(),
            sandbox_active: crate::args::SandboxChoice::Local,
            sandbox_remembered: None,
            sandbox_dialog: None,
            mcp: Vec::new(),
            mcp_dialog: None,
            mcp_confirm: None,
            websearch: Vec::new(),
            websearch_dialog: None,
            websearch_confirm: None,
            decision: Vec::new(),
            decision_dialog: None,
            decision_confirm: None,
            instance_confirm: None,
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
            update: UpdatePage::default(),
            config_path: None,
            workspace: String::new(),
            profile_count: 0,
        }
    }

    /// Lands on `section`: the page's row selection restarts at the
    /// top, and the Models page puts its cursor on the model in use, so
    /// Enter on an untouched page re-picks instead of jumping.
    pub fn focus_section(&mut self, section: SettingsSection) {
        self.section = section;
        self.row = 0;
        self.seed_model_cursor();
    }

    /// Where the model the profile runs sits in the picker's rows.
    fn current_model_index(&self) -> Option<usize> {
        let provider = self.stored.provider_name()?;
        let id = self.stored.model.clone()?;
        self.model_picker
            .rows()
            .iter()
            .position(|row| row.provider() == provider && row.id() == id)
    }

    /// Puts the Models page's cursor on the row of the model in use.
    fn seed_model_cursor(&mut self) {
        if self.section == SettingsSection::Models
            && let Some(index) = self.current_model_index()
        {
            self.model_picker.selected = index;
        }
    }

    /// Reads the picker's current-model mark off the page's stored
    /// profile, so the list names the model that runs.
    pub fn mark_current_model(&mut self) {
        let current = self
            .stored
            .provider_name()
            .zip(self.stored.model.clone())
            .map(|(provider, id)| CurrentModel {
                provider,
                instance: self.stored.instance.clone(),
                id,
            });
        self.model_picker.set_current(current);
    }

    /// The mark, plus the cursor: after a pick the Models page lands on
    /// the model it just made current.
    pub fn sync_current_model(&mut self) {
        self.mark_current_model();
        self.seed_model_cursor();
    }

    /// Puts the page's cursor on row `index`, whatever the page lists:
    /// the Models rows are the picker's.
    pub fn select_row(&mut self, index: usize) {
        if self.section == SettingsSection::Models {
            self.model_picker.selected = index;
        } else {
            self.row = index;
        }
    }

    /// Steps the sidebar one section, wrapping at the ends.
    fn step_section(&mut self, forward: bool) {
        let count = SettingsSection::ALL.len() as isize;
        let current = SettingsSection::ALL
            .iter()
            .position(|section| *section == self.section)
            .unwrap_or(0) as isize;
        let step = if forward { 1 } else { -1 };
        let section = SettingsSection::ALL[(current + step).rem_euclid(count) as usize];
        self.focus_section(section);
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

    /// The name of the highlighted instance row; `None` on the add
    /// row, with the removal confirm up, or anywhere but the Providers
    /// page.
    pub fn selected_instance(&self) -> Option<&str> {
        if self.section != SettingsSection::Providers
            || self.dialog.is_some()
            || self.instance_confirm.is_some()
        {
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

    /// The highlighted local row; `Some` only there, instance rows
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

    /// The highlighted web search row's name; `None` on the add row,
    /// with the dialog or the removal confirm up, or anywhere but the
    /// Web search page.
    pub fn selected_websearch(&self) -> Option<&str> {
        if self.section != SettingsSection::WebSearch
            || self.websearch_dialog.is_some()
            || self.websearch_confirm.is_some()
        {
            return None;
        }
        self.websearch
            .get(self.row)
            .and_then(|row| (!row.add).then_some(row.name.as_str()))
    }

    /// The highlighted decision row's name; `None` on the add row,
    /// with the dialog or the removal confirm up, or anywhere but the
    /// Decision models page.
    pub fn selected_decision(&self) -> Option<&str> {
        if self.section != SettingsSection::Decision
            || self.decision_dialog.is_some()
            || self.decision_confirm.is_some()
        {
            return None;
        }
        self.decision
            .get(self.row)
            .and_then(|row| (!row.add).then_some(row.name.as_str()))
    }

    /// How many interactive rows the page holds; zero means the arrows
    /// have nothing to move through.
    fn row_count(&self) -> usize {
        match self.section {
            SettingsSection::Providers => self.instance_rows().len(),
            SettingsSection::Profiles => self.profiles.len() + 1,
            SettingsSection::Sandbox => self.sandbox_rows().len(),
            SettingsSection::Appearance => 6,
            SettingsSection::Skills => self.skills.len(),
            SettingsSection::Subagents => self.subagents.len(),
            SettingsSection::Mcp => self.mcp.len() + 1,
            // The web search and decision rows already carry their add
            // row; the MCP rows do not, hence its `+ 1`.
            SettingsSection::WebSearch => self.websearch.len(),
            SettingsSection::Decision => self.decision.len(),
            SettingsSection::Update => 2,
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
                2 => SettingsAction::ToggleBackground,
                3 => SettingsAction::ToggleIcons,
                4 => SettingsAction::CycleCursor,
                _ => SettingsAction::ToggleScrollbar,
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
            SettingsSection::WebSearch => match self.selected_websearch() {
                Some(_) => SettingsAction::None,
                None => SettingsAction::OpenWebSearchDialog,
            },
            SettingsSection::Decision => match self.selected_decision() {
                Some(_) => SettingsAction::SetDefaultDecision,
                None => SettingsAction::OpenDecisionDialog,
            },
            // The Models page has one row per catalog entry, and Enter
            // picks the one under the cursor.
            SettingsSection::Models => SettingsAction::ChooseModel,
            SettingsSection::Update => match self.row {
                0 => SettingsAction::CheckForUpdates,
                _ if self.update.update_available() => SettingsAction::InstallUpdate,
                _ => SettingsAction::None,
            },
            _ => SettingsAction::None,
        }
    }

    /// Handles one keypress. The left/right arrows and tabs move the
    /// sidebar, the vertical arrows move the page's rows, esc or `q`
    /// backs out (dialog first, then the screen), enter acts on the
    /// selected row, `d` removes: a profile at once, an instance or an
    /// MCP server after its confirm, `c` cycles a profile's name
    /// color (and colors the Appearance page),
    /// `e` edits the selected subagent, space toggles a skill or
    /// subagent or mutes an MCP server, and the Appearance page carries
    /// `t`/`b`/`i`/`s`/`r` as shortcuts for its remaining toggles.
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
                DialogAction::OpenFlowUrl => SettingsAction::OpenFlowUrl,
                DialogAction::CopyFlowUrl => SettingsAction::CopyFlowUrl,
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
            .websearch_dialog
            .as_mut()
            .map(|dialog| dialog.handle_key(key))
        {
            if action == WebSearchDialogAction::Close {
                self.websearch_dialog = None;
            }
            return match action {
                WebSearchDialogAction::None => SettingsAction::None,
                WebSearchDialogAction::Submit => SettingsAction::SubmitWebSearchDialog,
                WebSearchDialogAction::Close => SettingsAction::CloseWebSearchDialog,
            };
        }
        if let Some(action) = self
            .decision_dialog
            .as_mut()
            .map(|dialog| dialog.handle_key(key))
        {
            if action == DecisionDialogAction::Close {
                self.decision_dialog = None;
            }
            return match action {
                DecisionDialogAction::None => SettingsAction::None,
                DecisionDialogAction::Submit => SettingsAction::SubmitDecisionDialog,
                DecisionDialogAction::Close => SettingsAction::CloseDecisionDialog,
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
        if self.websearch_confirm.is_some() {
            // The removal confirm takes every key while it is up: esc
            // must stay here, not back out of the screen behind it.
            return match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.websearch_confirm = None;
                    SettingsAction::RemoveWebSearchProvider
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.websearch_confirm = None;
                    SettingsAction::None
                }
                _ => SettingsAction::None,
            };
        }
        if self.decision_confirm.is_some() {
            // The removal confirm takes every key while it is up: esc
            // must stay here, not back out of the screen behind it.
            return match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.decision_confirm = None;
                    SettingsAction::RemoveDecisionProvider
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.decision_confirm = None;
                    SettingsAction::None
                }
                _ => SettingsAction::None,
            };
        }
        if self.instance_confirm.is_some() {
            // The removal confirm takes every key while it is up: esc
            // must stay here, not back out of the screen behind it.
            return match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.instance_confirm = None;
                    SettingsAction::RemoveInstance
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.instance_confirm = None;
                    SettingsAction::None
                }
                _ => SettingsAction::None,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return SettingsAction::None;
        }
        // The Models page's rows are the shared picker's: it takes every
        // key, so typing filters, the vertical arrows walk the catalog,
        // and the page has one cursor rather than two. The sidebar keys
        // stay the page's, or the page would be a trap.
        if self.section == SettingsSection::Models
            && !matches!(
                key.code,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right
            )
        {
            return match self.model_picker.handle_key(key) {
                ModelPickerAction::None => SettingsAction::None,
                ModelPickerAction::Back => SettingsAction::Back,
                ModelPickerAction::Retry => SettingsAction::RetryModels,
                ModelPickerAction::Choose { .. } => SettingsAction::ChooseModel,
            };
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
                // Removal asks first: the confirm modal arms here and
                // the y that answers it carries the removal out.
                if let Some(name) = self.selected_instance() {
                    self.instance_confirm = Some(name.to_owned());
                }
                SettingsAction::None
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
            KeyCode::Char(' ') if self.selected_subagent().is_some() => {
                SettingsAction::ToggleSubagent
            }
            KeyCode::Char(' ') if self.selected_mcp_server().is_some() => {
                SettingsAction::ToggleMcpServer
            }
            KeyCode::Char('d') if self.selected_mcp_server().is_some() => {
                // Removal asks first: the confirm modal arms here and
                // the y that answers it carries the removal out.
                if let Some(name) = self.selected_mcp_server() {
                    self.mcp_confirm = Some(name.to_owned());
                }
                SettingsAction::None
            }
            KeyCode::Char('d') if self.selected_websearch().is_some() => {
                // Removal asks first, the same modal the other pages
                // use; the y that answers it carries the removal out.
                if let Some(name) = self.selected_websearch() {
                    self.websearch_confirm = Some(name.to_owned());
                }
                SettingsAction::None
            }
            KeyCode::Char('d') if self.selected_decision().is_some() => {
                // Removal asks first, the same modal the other pages
                // use; the y that answers it carries the removal out.
                if let Some(name) = self.selected_decision() {
                    self.decision_confirm = Some(name.to_owned());
                }
                SettingsAction::None
            }
            // j and k move the row under the cursor through the
            // chain, vim's pair: k lifts it one step up the chain
            // (an unordered row lifts into the chain, last), j drops
            // it one step down (never out of it). Arrows keep
            // navigating, the page's rows stay walked with them.
            KeyCode::Char('k') if self.selected_websearch().is_some() => {
                SettingsAction::MoveWebSearchUp
            }
            KeyCode::Char('j') if self.selected_websearch().is_some() => {
                SettingsAction::MoveWebSearchDown
            }
            KeyCode::Char('c') if self.section == SettingsSection::Appearance => {
                SettingsAction::CyclePalette
            }
            KeyCode::Char('t') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleTheme
            }
            KeyCode::Char('b') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleBackground
            }
            KeyCode::Char('i') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleIcons
            }
            KeyCode::Char('s') if self.section == SettingsSection::Appearance => {
                SettingsAction::CycleCursor
            }
            KeyCode::Char('r') if self.section == SettingsSection::Appearance => {
                SettingsAction::ToggleScrollbar
            }
            _ => SettingsAction::None,
        }
    }
}
