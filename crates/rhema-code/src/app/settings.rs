//! The settings and onboarding screens: what each action does to the
//! app's state, plus the theme, palette, icon, skill, and subagent
//! toggles the pages drive.

use super::*;

impl App {
    /// Opens the settings screen on `section`. `/settings` and `/login`
    /// both land on Providers — the login flow lives there now. The
    /// Providers page edits the session's profile live; the other pages
    /// read snapshots of this moment. Openable mid-turn: only the
    /// instance actions wait for an idle session.
    pub fn open_settings_at(&mut self, section: SettingsSection) {
        let mut settings = Self::build_settings_screen(
            &self.config,
            &self.options,
            self.session_profile.as_deref(),
            self.home.as_deref(),
            self.config_root.as_deref(),
            self.secret_store(),
        );
        settings.section = section;
        settings.allowance = self.subscription.clone();
        settings.last_turn = self.usage;
        settings.transcript = self
            .agent
            .as_ref()
            .and_then(CodingAgent::transcript_stats)
            .or(self.transcript_stats);
        settings.session_cost = self.session_cost;
        settings.sandbox_offered = self.sandbox_offered;
        settings.sandbox_active = self.active_sandbox();
        settings.sandbox_remembered = self
            .config
            .last_sandbox_in(&self.options.workspace)
            .as_deref()
            .and_then(|kind| SandboxChoice::from_stored(Some(kind)));
        settings.mcp = crate::screens::mcp_server_rows(
            &settings.stored.mcp_servers,
            &self.disabled_mcp_servers,
        );
        self.screen = Screen::Settings(Box::new(settings));
    }

    /// Assembles the settings screen: the live Providers page over the
    /// session's profile, plus the other pages' snapshots.
    pub(crate) fn build_settings_screen(
        config: &Config,
        options: &RunOptions,
        session_profile: Option<&str>,
        home: Option<&Path>,
        config_root: Option<&Path>,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> SettingsScreen {
        let effective = config.effective(session_profile);
        let workspace =
            std::fs::canonicalize(&options.workspace).unwrap_or_else(|_| options.workspace.clone());
        let agents_dir = home.map(|home| rhema_config::Home::under_home(home).agents_skills());
        let global_skills = config_root.map(|root| root.join("skills"));
        let skills = skill_rows(
            agents_dir.as_deref(),
            global_skills.as_deref(),
            &workspace,
            &config.skills_disabled,
        );
        // The same discovery the agent's wiring runs, so the page shows
        // what the next session will delegate to.
        let global_subagents = config_root.map(|root| root.to_path_buf());
        let mut settings = SettingsScreen::new();
        settings.colors = options.colors;
        settings.theme_dark = options.theme == crate::palette::ThemeKind::Dark;
        settings.icons_nerd = options.icons == crate::icons::Flavor::Nerd;
        settings.background_native = options.background == crate::palette::Background::Native;
        settings.profile = session_profile
            .unwrap_or(rhema_config::DEFAULT_PROFILE)
            .to_owned();
        settings.stored = effective;
        settings.profiles =
            crate::screens::profile_rows(&config.profiles, session_profile, secrets);
        settings.skills = skills;
        settings.skills_global_dir = global_skills.map(|dir| dir.to_string_lossy().into_owned());
        settings.skills_workspace_dir = workspace
            .join(".rhema")
            .join("skills")
            .to_string_lossy()
            .into_owned();
        settings.subagents = subagent_rows(
            global_subagents.as_deref(),
            &workspace,
            &config.disabled_subagents,
        );
        settings.subagents_global_dir =
            global_subagents.map(|dir| dir.to_string_lossy().into_owned());
        settings.subagents_workspace_dir = workspace
            .join(".rhema")
            .join("subagents")
            .to_string_lossy()
            .into_owned();
        settings.config_path = rhema_config::Home::discover()
            .map(|home| home.config_file())
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        settings.workspace = workspace.to_string_lossy().into_owned();
        settings.profile_count = config.profiles.len();
        settings
    }

    /// Re-reads the Subagents page's snapshot: an edit happened in
    /// `$EDITOR` behind the app's back, and the rows must show it when
    /// the TUI re-enters. A no-op when the settings screen is not open.
    pub fn refresh_subagents(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        let workspace = std::fs::canonicalize(&self.options.workspace)
            .unwrap_or_else(|_| self.options.workspace.clone());
        let global = self.config_root.clone();
        settings.subagents = subagent_rows(
            global.as_deref(),
            &workspace,
            &self.config.disabled_subagents,
        );
        settings.subagents_global_dir = global.map(|dir| dir.to_string_lossy().into_owned());
        settings.subagents_workspace_dir = workspace
            .join(".rhema")
            .join("subagents")
            .to_string_lossy()
            .into_owned();
        if settings.row >= settings.subagents.len() {
            settings.row = 0;
        }
    }

    /// Rebuilds the MCP page's snapshot from the config and the
    /// session's muted set, keeping the selection inside the shrunken
    /// list. A no-op when the settings screen is not open.
    pub(crate) fn refresh_mcp_page(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        settings.mcp_dialog = None;
        settings.stored = self.config.effective(self.session_profile.as_deref());
        settings.mcp = crate::screens::mcp_server_rows(
            &settings.stored.mcp_servers,
            &self.disabled_mcp_servers,
        );
        let rows = settings.mcp.len() + 1;
        if settings.row >= rows {
            settings.row = rows.saturating_sub(1);
        }
    }

    /// Enter on the MCP page's add row: the dialog opens, refusing the
    /// profile's current server names.
    pub(crate) fn open_mcp_dialog(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        let taken = settings.stored.mcp_servers.keys().cloned().collect();
        settings.mcp_dialog = Some(crate::screens::AddMcpServerDialog::new(taken));
    }

    /// Flips a server's muted state: the config's
    /// `disabled_mcp_servers` set flips with the session's live copy
    /// (the `skills_disabled` twin, now a config field), the change
    /// persists through save_config, and the running agent rebuilds
    /// carrying the conversation over — the prompt carries the MCP
    /// section, so the very next turn sees it. The open row follows.
    pub(crate) fn toggle_mcp_server(&mut self, name: &str) {
        let muted = !self.disabled_mcp_servers.contains(name);
        if muted {
            self.disabled_mcp_servers.insert(name.to_owned());
        } else {
            self.disabled_mcp_servers.remove(name);
        }
        if muted {
            self.config.disabled_mcp_servers.insert(name.to_owned());
        } else {
            self.config.disabled_mcp_servers.remove(name);
        }
        if let Screen::Settings(settings) = &mut self.screen {
            for row in &mut settings.mcp {
                if row.name == name {
                    row.disabled = muted;
                }
            }
        }
        let notice = if muted {
            format!("the {name} server is muted — saved and applied to new turns")
        } else {
            format!("the {name} server is unmuted — saved and applied to new turns")
        };
        self.transcript.push_notice(&notice);
        if self.save_config() {
            // The prompt and the shell env both carry the wiring, so
            // the running agent rebuilds exactly like a skill toggle;
            // a running turn defers the rebuild to its end.
            let _ = self.rebuild_live_agent(false);
        }
    }

    /// The add-server dialog's accepted entry: it lands in the
    /// profile's `mcp_servers` map through the same profile write the
    /// other settings pages use, and the snapshot follows.
    pub(crate) fn submit_mcp_server(&mut self) -> Vec<Effect> {
        let built = match &self.screen {
            Screen::Settings(settings) => settings
                .mcp_dialog
                .as_ref()
                .and_then(|dialog| dialog.build().ok()),
            _ => None,
        };
        let Some((name, entry)) = built else {
            return Vec::new();
        };
        // The dialog dedupes against its opening snapshot; this is
        // the belt for a path that should not exist.
        if self.effective().mcp_servers.contains_key(&name) {
            self.transcript
                .push_notice(&format!("a server named {name} already exists"));
            return Vec::new();
        }
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                profile.mcp_servers.insert(name.clone(), entry);
            });
        if !self.save_config() {
            return Vec::new();
        }
        self.refresh_mcp_page();
        // The tool surface changed, so the cached wiring is stale:
        // drop it and rebuild, and the very next turn's prompt lists
        // the new server. Same shape as the mute toggle's rebuild —
        // the conversation rides over.
        crate::mcp_wire::rewire(self.mcp_session_key().as_str());
        let _ = self.rebuild_live_agent(false);
        self.transcript.push_notice(&format!(
            "added the {name} server to this profile — the agent rebuilt with it"
        ));
        Vec::new()
    }

    /// The MCP registry key for this session: the profile's name, the
    /// same key `session_agent_config` wired under.
    pub(crate) fn mcp_session_key(&self) -> String {
        self.session_profile
            .clone()
            .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned())
    }

    /// `y` on the removal confirm: the entry leaves the profile's map
    /// and the config persists, the way the sandbox instance removal
    /// writes. A muted name dies with its server — nothing left to
    /// mute.
    pub(crate) fn remove_mcp_server(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_mcp_server().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let mut removed = false;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                removed = profile.mcp_servers.remove(&name).is_some();
            });
        if !removed {
            return Vec::new();
        }
        if !self.save_config() {
            return Vec::new();
        }
        self.disabled_mcp_servers.remove(&name);
        self.refresh_mcp_page();
        // The surface shrank: drop the wiring so the rebuilt agent
        // stops advertising the removed server.
        crate::mcp_wire::rewire(self.mcp_session_key().as_str());
        let _ = self.rebuild_live_agent(false);
        self.transcript
            .push_notice(&format!("removed the {name} server"));
        Vec::new()
    }

    /// Opens the selected subagent definition in `$EDITOR`. The built-in
    /// has no file: the first edit copies it into the global directory,
    /// where a definition of the same name shadows the built-in from
    /// then on. The editor itself runs in the runner — the TUI must be
    /// suspended around it — and the rows refresh when it returns.
    pub(crate) fn edit_subagent(&mut self) -> Vec<Effect> {
        let Screen::Settings(settings) = &self.screen else {
            return Vec::new();
        };
        let Some(row) = settings.selected_subagent() else {
            return Vec::new();
        };
        if let Some(path) = &row.path {
            return vec![Effect::EditSubagent(path.clone())];
        }
        // Only the built-in row reaches here; its shadow lands in the
        // global directory discovery reads first.
        match rhema_config::Home::discover().map(|home| home.config) {
            Ok(dir) => match shadow_builtin(&dir, &row.name) {
                Ok(path) => vec![Effect::EditSubagent(path)],
                Err(err) => {
                    self.transcript
                        .push_notice(&format!("cannot copy the built-in definition: {err}"));
                    Vec::new()
                }
            },
            Err(err) => {
                self.transcript.push_notice(&format!(
                    "no config directory to copy the built-in into: {err}"
                ));
                Vec::new()
            }
        }
    }

    /// Flips the appearance and persists the choice: the swap is live
    /// (the tokens change this frame), and the config records it for the
    /// next startup. The open settings page follows, so it keeps telling
    /// the truth.
    pub(crate) fn toggle_theme(&mut self) {
        let dark = self.options.theme == crate::palette::ThemeKind::Dark;
        let kind = if dark {
            crate::palette::ThemeKind::Light
        } else {
            crate::palette::ThemeKind::Dark
        };
        self.options.theme = kind;
        self.theme = self.options.resolved_theme();
        self.config.theme = Some(if dark { "light" } else { "dark" }.to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.theme_dark = !dark;
        }
        self.save_config();
    }

    /// Flips the background source and persists the choice, live like
    /// the theme toggle: `native` hands the screen's background back to
    /// the terminal, `rhema` paints the palette's fills again. The open
    /// settings page follows.
    pub(crate) fn toggle_background(&mut self) {
        let native = self.options.background == crate::palette::Background::Native;
        let background = if native {
            crate::palette::Background::Rhema
        } else {
            crate::palette::Background::Native
        };
        self.options.background = background;
        self.theme = self.options.resolved_theme();
        self.config.background = Some(background.stored_name().to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.background_native = background == crate::palette::Background::Native;
        }
        self.save_config();
    }

    /// Cycles to the next palette and persists the choice, live like the
    /// theme toggle. The open settings page follows.
    pub(crate) fn cycle_palette(&mut self) {
        let all = crate::palette::Palette::ALL;
        let current = all
            .iter()
            .position(|palette| *palette == self.options.colors)
            .unwrap_or(0);
        self.set_palette(all[(current + 1) % all.len()]);
    }

    /// Applies one palette live — the frame this call renders already
    /// wears it — and persists the choice. The wizard's Colors step and
    /// the settings' Appearance page both land here; the open screen
    /// follows, so the rows keep telling the truth.
    pub(crate) fn set_palette(&mut self, palette: crate::palette::Palette) {
        self.options.colors = palette;
        self.theme = self.options.resolved_theme();
        self.config.colors = Some(palette.stored_name().to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.colors = palette;
        }
        self.save_config();
    }

    /// Flips the footer's icon flavor and persists the choice, live like
    /// the theme toggle.
    pub(crate) fn toggle_icons(&mut self) {
        let nerd = self.options.icons != crate::icons::Flavor::Nerd;
        let flavor = if nerd {
            crate::icons::Flavor::Nerd
        } else {
            crate::icons::Flavor::Unicode
        };
        self.options.icons = flavor;
        self.icons = flavor.set();
        self.config.icons = Some(if nerd { "nerd" } else { "unicode" }.to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.icons_nerd = nerd;
        }
        self.save_config();
    }

    /// Flips a skill's on/off state: the disabled set in the config
    /// flips, a bundled skill being switched back on is re-seeded when
    /// its folder is gone, and the running agent rebuilds carrying the
    /// conversation over — the system prompt carries the skills list,
    /// so the very next turn sees the change. The open screen's row
    /// follows (settings page or wizard step); a running turn defers
    /// the rebuild to its end, the toggle itself lands immediately.
    pub(crate) fn toggle_skill(&mut self, name: &str) {
        let turning_on = self.config.skills_disabled.contains(name);
        if turning_on {
            self.config.skills_disabled.remove(name);
        } else {
            self.config.skills_disabled.insert(name.to_owned());
        }
        // A bundled skill switched back on must exist on disk again —
        // seeding skips disabled names, so its folder may be gone.
        if turning_on && let Some(root) = self.config_root.clone() {
            let _ = crate::bundled::seed_one(&root.join("skills"), name);
        }
        if !self.save_config() {
            return;
        }
        // The open screen's row follows: the settings page or the
        // wizard step, whichever is showing.
        match &mut self.screen {
            Screen::Settings(settings) => {
                for row in &mut settings.skills {
                    if row.name == name {
                        row.enabled = !row.enabled;
                    }
                }
            }
            Screen::Onboarding(wizard) => {
                for row in &mut wizard.skills {
                    if row.name == name {
                        row.enabled = !row.enabled;
                    }
                }
            }
            _ => {}
        }
        // The prompt change rides the next agent build; mid-turn the
        // rebuild would race the running one, so it waits.
        if !self.turn.running
            && let Err(err) = self.rebuild_live_agent(false)
        {
            self.error = Some(err);
        }
    }

    /// Flips a subagent definition's on/off state: the config's
    /// `disabled_subagents` set flips, the change persists, and the
    /// running agent rebuilds carrying the conversation over — the
    /// `subagent` tool's catalog is in the system prompt, so the very
    /// next turn sees the change. A set left holding every name (the
    /// built-in included) drops the tool whole. The open screen's row
    /// follows; a running turn defers the rebuild to its end, the
    /// toggle itself lands immediately.
    pub(crate) fn toggle_subagent(&mut self, name: &str) {
        let turning_on = self.config.disabled_subagents.contains(name);
        if turning_on {
            self.config.disabled_subagents.remove(name);
        } else {
            self.config.disabled_subagents.insert(name.to_owned());
        }
        if !self.save_config() {
            return;
        }
        if let Screen::Settings(settings) = &mut self.screen {
            for row in &mut settings.subagents {
                if row.name == name {
                    row.enabled = !row.enabled;
                }
            }
        }
        // The prompt change rides the next agent build; mid-turn the
        // rebuild would race the running one, so it waits.
        if !self.turn.running
            && let Err(err) = self.rebuild_live_agent(false)
        {
            self.error = Some(err);
        }
    }

    /// The profile step's accepted rename: the config's profile entry
    /// moves to the new name (its settings ride along), the session
    /// follows, and the workspace's profile memory forgets the old
    /// name. A blank or unchanged name never reaches here.
    pub(crate) fn rename_session_profile(&mut self, new_name: &str) {
        let Some(old) = self.session_profile.clone() else {
            return;
        };
        if new_name == old || self.config.profiles.contains_key(new_name) {
            return;
        }
        let entry = self.config.profiles.remove(&old).unwrap_or_default();
        self.config.profiles.insert(new_name.to_owned(), entry);
        self.session_profile = Some(new_name.to_owned());
        self.config
            .last_profile_by_workspace
            .retain(|_, remembered| remembered != &old);
        self.save_config();
    }

    /// The system prompt in the transcript, fenced so it reads verbatim.
    pub fn show_system_prompt(&mut self, prompt: &str) {
        self.transcript
            .push_markdown(format!("```markdown\n{prompt}```"));
    }
}
