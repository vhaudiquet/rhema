//! The settings and onboarding screens: what each action does to the
//! app's state, plus the theme, palette, icon, skill, and subagent
//! toggles the pages drive.

use super::*;

impl App {
    /// Opens the settings screen on `section`. `/settings` and `/login`
    /// both land on Providers, the login flow lives there now. The
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
        settings.allowance = self.subscription.clone();
        settings.last_turn = self.usage;
        settings.transcript = self
            .agent
            .as_ref()
            .and_then(HostAgent::transcript_stats)
            .or(self.transcript_stats);
        settings.session_cost = self.session_cost;
        settings.model_picker.set_models(self.models.clone());
        settings.model_picker.unavailable = self.catalog_unavailable.clone();
        settings.model_picker.failed = self.catalog_failed.clone();
        settings.sandbox_offered = self.sandbox_offered;
        settings.sandbox_active = self.active_sandbox();
        settings.sandbox_remembered = self
            .config
            .last_sandbox_in(&self.options.workspace)
            .as_deref()
            .and_then(|kind| SandboxChoice::from_stored(Some(kind)));
        settings.update = self.update.clone();
        settings.mcp = crate::screens::mcp_server_rows(
            &settings.stored.mcp_servers,
            &self.disabled_mcp_servers,
        );
        settings.websearch = crate::screens::websearch_rows(
            &settings.stored.websearch,
            &settings.stored.websearch_order,
        );
        settings.decision = crate::screens::decision_rows(
            settings.stored.decision.as_ref(),
            self.secrets.as_deref(),
        );
        settings.sync_current_model();
        settings.focus_section(section);
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
        settings.cursor = options.cursor;
        settings.scrollbar = options.scrollbar;
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

    /// Copies the update state into the open settings page, so the
    /// Update page keeps telling the truth while a check or an install
    /// runs behind it. A no-op when the screen is not open.
    pub(crate) fn sync_update_page(&mut self) {
        if let Screen::Settings(settings) = &mut self.screen {
            settings.update = self.update.clone();
        }
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
    /// carrying the conversation over, the prompt carries the MCP
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
            format!("the {name} server is muted, saved and applied to new turns")
        } else {
            format!("the {name} server is unmuted, saved and applied to new turns")
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
        // the new server. Same shape as the mute toggle's rebuild,
        // the conversation rides over.
        crate::mcp_wire::rewire(self.mcp_session_key().as_str());
        let _ = self.rebuild_live_agent(false);
        self.transcript.push_notice(&format!(
            "added the {name} server to this profile, the agent rebuilt with it"
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
    /// writes. A muted name dies with its server, nothing left to
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

    /// Rebuilds the web search page's snapshot from the config,
    /// keeping the selection inside the shrunken list. A no-op when
    /// the settings screen is not open.
    pub(crate) fn refresh_websearch_page(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        settings.websearch_dialog = None;
        settings.stored = self.config.effective(self.session_profile.as_deref());
        settings.websearch = crate::screens::websearch_rows(
            &settings.stored.websearch,
            &settings.stored.websearch_order,
        );
        // The rows carry their add row; the MCP equivalent below has
        // to add one because its rows do not.
        let rows = settings.websearch.len();
        if settings.row >= rows {
            settings.row = rows.saturating_sub(1);
        }
    }

    /// Enter on the web search page's add row: the dialog opens,
    /// refusing the profile's current instance names.
    pub(crate) fn open_websearch_dialog(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        let taken = settings.stored.websearch.keys().cloned().collect();
        settings.websearch_dialog = Some(crate::screens::AddWebSearchDialog::new(taken));
    }

    /// The add-provider dialog's accepted provider: the instance
    /// lands in the profile's `websearch` map, the key goes through
    /// [`rhema_config::Profile::place_websearch_key`] (keychain first,
    /// the literal when no store accepts it), the name joins the
    /// chain order at the end, and the config persists. The running
    /// agent rebuilds, because a first provider flips the wiring
    /// gate: the function and the section exist from the next turn.
    pub(crate) fn submit_websearch_provider(&mut self) -> Vec<Effect> {
        let built = match &self.screen {
            Screen::Settings(settings) => settings
                .websearch_dialog
                .as_ref()
                .and_then(|dialog| dialog.build().ok()),
            _ => None,
        };
        let Some((name, kind, key)) = built else {
            return Vec::new();
        };
        // The dialog dedupes against its opening snapshot; this is
        // the belt for a path that should not exist.
        if self.effective().websearch.contains_key(&name) {
            self.transcript
                .push_notice(&format!("a provider named {name} already exists"));
            return Vec::new();
        }
        let secrets = self.secrets.clone();
        let mut location = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                profile.websearch.insert(
                    name.clone(),
                    rhema_config::WebSearchInstance {
                        provider: kind.to_owned(),
                        api_key: None,
                        secret_ref: None,
                    },
                );
                location = profile.place_websearch_key(&name, &key, secrets.as_deref());
                profile.websearch_order.push(name.clone());
            });
        if !self.save_config() {
            return Vec::new();
        }
        self.refresh_websearch_page();
        let where_note = match location {
            Some(rhema_config::KeyLocation::Keychain) => "the key went to the keychain",
            _ => "the key stayed in the config file",
        };
        let _ = self.rebuild_live_agent(false);
        self.transcript.push_notice(&format!(
            "added {name} ({kind}) to this profile, {where_note}, the agent rebuilt with it"
        ));
        Vec::new()
    }

    /// `y` on the removal confirm: the instance leaves the profile's
    /// map, its order entry goes with it, and the config persists.
    /// The running agent rebuilds when the last provider went: the
    /// wiring gate closes, the function and the section go away.
    pub(crate) fn remove_websearch_provider(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_websearch().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let mut removed = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                removed = profile.websearch.remove(&name);
                profile.websearch_order.retain(|entry| entry != &name);
            });
        let Some(removed) = removed else {
            return Vec::new();
        };
        // The keychain entry goes with the instance, the same rule the
        // provider logins follow: a removed key must not stay in the
        // chain. Best effort; a store that refuses leaves the entry,
        // and that must not pass silently.
        if let Some(id) = removed.secret_ref
            && let Some(store) = self.secret_store()
            && let Err(err) = store.delete(&id)
        {
            self.error = Some(format!(
                "the provider is removed, but its keychain entry could not be deleted: {err}"
            ));
        }
        if !self.save_config() {
            return Vec::new();
        }
        self.refresh_websearch_page();
        let empty_now = self.effective().websearch.is_empty();
        if !empty_now || !self.turn.running {
            let _ = self.rebuild_live_agent(false);
        }
        self.transcript
            .push_notice(&format!("removed the {name} provider"));
        Vec::new()
    }

    /// `[` or `]` on a provider row: one step up or down the chain.
    /// `[` on an unordered row brings it into the chain at the end.
    /// Persisted with no rebuild: the chain order is read fresh per
    /// `search` invocation, the wiring carries nothing of it.
    pub(crate) fn move_websearch_provider(&mut self, up: bool) {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_websearch().map(str::to_owned),
            _ => None,
        }) else {
            return;
        };
        let mut moved = false;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let Some(pos) = profile.websearch_order.iter().position(|n| n == &name) else {
                    // An unordered row: `[` enrolls it, last in the
                    // chain. `]` does nothing, out of the chain is
                    // not a keypress away.
                    if up {
                        profile.websearch_order.push(name.clone());
                        moved = true;
                    }
                    return;
                };
                let new = if up {
                    pos.saturating_sub(1)
                } else {
                    (pos + 1).min(profile.websearch_order.len() - 1)
                };
                if new != pos {
                    profile.websearch_order.remove(pos);
                    profile.websearch_order.insert(new, name.clone());
                    moved = true;
                }
            });
        if !moved || !self.save_config() {
            return;
        }
        self.refresh_websearch_page();
        self.transcript
            .push_notice(&format!("moved {name} in the search chain, saved"));
    }

    /// Rebuilds the Decision models page's snapshot from the config,
    /// keeping the selection inside the shrunken list. A no-op when
    /// the settings screen is not open.
    pub(crate) fn refresh_decision_page(&mut self) {
        let secrets = self.secrets.clone();
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        settings.decision_dialog = None;
        settings.stored = self.config.effective(self.session_profile.as_deref());
        settings.decision =
            crate::screens::decision_rows(settings.stored.decision.as_ref(), secrets.as_deref());
        let rows = settings.decision.len();
        if settings.row >= rows {
            settings.row = rows.saturating_sub(1);
        }
    }

    /// Enter on the Decision models page's add row: the dialog opens,
    /// refusing the profile's current instance names.
    pub(crate) fn open_decision_dialog(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        let taken = settings
            .stored
            .decision
            .as_ref()
            .map(|section| section.instances.keys().cloned().collect())
            .unwrap_or_default();
        settings.decision_dialog = Some(crate::screens::AddDecisionDialog::new(taken));
    }

    /// The add-instance dialog's accepted instance: it lands in the
    /// profile's `decision` section, the key goes through
    /// [`rhema_config::Profile::place_decision_key`] (keychain first,
    /// the literal when no store accepts it), and the config
    /// persists. The running agent rebuilds, because a first instance
    /// flips the wiring gate: the function and the section exist from
    /// the next turn.
    pub(crate) fn submit_decision_provider(&mut self) -> Vec<Effect> {
        let built = match &self.screen {
            Screen::Settings(settings) => settings
                .decision_dialog
                .as_ref()
                .and_then(|dialog| dialog.build().ok()),
            _ => None,
        };
        let Some(built) = built else {
            return Vec::new();
        };
        // The dialog dedupes against its opening snapshot; this is
        // the belt for a path that should not exist.
        if self
            .effective()
            .decision
            .as_ref()
            .is_some_and(|section| section.instances.contains_key(&built.name))
        {
            self.transcript
                .push_notice(&format!("an instance named {} already exists", built.name));
            return Vec::new();
        }
        let secrets = self.secrets.clone();
        let mut location = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let section = profile.decision.get_or_insert_with(Default::default);
                section.instances.insert(
                    built.name.clone(),
                    rhema_config::DecisionInstance {
                        provider: built.kind.to_owned(),
                        api_key: None,
                        secret_ref: None,
                        model: None,
                        account_id: built.account_id.clone(),
                        base_url: built.base_url.clone(),
                    },
                );
                location = profile.place_decision_key(&built.name, &built.key, secrets.as_deref());
            });
        if !self.save_config() {
            return Vec::new();
        }
        self.refresh_decision_page();
        let where_note = match location {
            Some(rhema_config::KeyLocation::Keychain) => "the key went to the keychain",
            _ => "the key stayed in the config file",
        };
        let _ = self.rebuild_live_agent(false);
        self.transcript.push_notice(&format!(
            "added {} ({}) to this profile, {where_note}, the agent rebuilt with it",
            built.name, built.kind
        ));
        Vec::new()
    }

    /// `y` on the removal confirm: the instance leaves the profile's
    /// section, the default goes with it when it was the one (the
    /// first by name serves, [`rhema_config::DecisionSettings::active_instance`]'s
    /// rule), an emptied section leaves the file, and the config
    /// persists. The running agent rebuilds when the last instance
    /// went: the wiring gate may close, the function and the section
    /// go away.
    pub(crate) fn remove_decision_provider(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_decision().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let mut removed = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let Some(section) = profile.decision.as_mut() else {
                    return;
                };
                removed = section.instances.remove(&name);
                if section.instance.as_deref() == Some(name.as_str()) {
                    section.instance = None;
                }
                if section.instances.is_empty() {
                    profile.decision = None;
                }
            });
        let Some(removed) = removed else {
            return Vec::new();
        };
        // The keychain entry goes with the instance, the same rule the
        // provider logins follow: a removed key must not stay in the
        // chain. Best effort; a store that refuses leaves the entry,
        // and that must not pass silently.
        if let Some(id) = removed.secret_ref
            && let Some(store) = self.secret_store()
            && let Err(err) = store.delete(&id)
        {
            self.error = Some(format!(
                "the instance is removed, but its keychain entry could not be deleted: {err}"
            ));
        }
        if !self.save_config() {
            return Vec::new();
        }
        self.refresh_decision_page();
        let empty_now = self
            .effective()
            .decision
            .as_ref()
            .is_none_or(|section| section.instances.is_empty());
        if !empty_now || !self.turn.running {
            let _ = self.rebuild_live_agent(false);
        }
        self.transcript
            .push_notice(&format!("removed the {name} decision model"));
        Vec::new()
    }

    /// Enter on an instance row: `decide` asks it by default. The
    /// mode reads the profile fresh per call, so the next call
    /// already sees it and no rebuild follows; the config persists.
    pub(crate) fn set_default_decision(&mut self) {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_decision().map(str::to_owned),
            _ => None,
        }) else {
            return;
        };
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                if let Some(section) = profile.decision.as_mut()
                    && section.instances.contains_key(&name)
                {
                    section.instance = Some(name.clone());
                }
            });
        if !self.save_config() {
            return;
        }
        self.refresh_decision_page();
        self.transcript
            .push_notice(&format!("decide asks {name} by default, saved"));
    }

    /// Opens the selected subagent definition in `$EDITOR`. The built-in
    /// has no file: the first edit copies it into the global directory,
    /// where a definition of the same name shadows the built-in from
    /// then on. The editor itself runs in the runner, the TUI must be
    /// suspended around it, and the rows refresh when it returns.
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

    /// Applies one palette live, the frame this call renders already
    /// wears it, and persists the choice. The wizard's Colors step and
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

    /// Cycles the input caret's shape and persists the choice, live
    /// like the theme toggle: the returned effect writes the new shape
    /// to the terminal this frame. The open settings page follows.
    pub(crate) fn cycle_cursor_style(&mut self) -> Effect {
        let style = self.options.cursor.next();
        self.options.cursor = style;
        self.config.cursor = Some(style.stored_name().to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.cursor = style;
        }
        self.save_config();
        Effect::SetCursorStyle(style)
    }

    /// Flips the transcript's scrollbar and persists the choice, live
    /// like the theme toggle: the rail stops drawing (or comes back)
    /// this frame, and the config records it for the next startup. The
    /// open settings page follows.
    pub(crate) fn toggle_scrollbar(&mut self) {
        let scrollbar = !self.options.scrollbar;
        self.options.scrollbar = scrollbar;
        self.config.scrollbar = Some(scrollbar);
        if let Screen::Settings(settings) = &mut self.screen {
            settings.scrollbar = scrollbar;
        }
        self.save_config();
    }

    /// Flips a skill's on/off state: the disabled set in the config
    /// flips, a bundled skill being switched back on is re-seeded when
    /// its folder is gone, and the running agent rebuilds carrying the
    /// conversation over, the system prompt carries the skills list,
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
        // A bundled skill switched back on must exist on disk again,
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
    /// running agent rebuilds carrying the conversation over, the
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

    /// Lands the open settings screen on the Models page, its picker
    /// seeded from the cached catalog: a login and an instance
    /// activation both end here, the pick persisting to the profile
    /// they just changed.
    pub(crate) fn focus_settings_models(&mut self) {
        let models = self.models.clone();
        let unavailable = self.catalog_unavailable.clone();
        let failed = self.catalog_failed.clone();
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        settings.model_picker.set_models(models);
        settings.model_picker.unavailable = unavailable;
        settings.model_picker.failed = failed;
        settings.model_picker.status = ModelPickerStatus::Ready;
        settings.mark_current_model();
        settings.focus_section(SettingsSection::Models);
    }

    /// `/model` and Ctrl-P: the Models page of the settings screen, its
    /// picker seeded from the cached catalog and refetched when the
    /// cache is empty or stale. One place picks a model, and the page
    /// under it is the one the pick writes to.
    pub(crate) fn open_model_settings(&mut self) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; the model pick waits for it to finish");
            return Vec::new();
        }
        // No candidate, no list: the page would open with nothing in it
        // and nothing saying why, and the run would drop the fetch.
        if Backend::candidates_from_profile(&self.effective(), self.secret_store()).is_empty() {
            self.transcript
                .push_notice("no provider on this profile; /login first");
            return Vec::new();
        }
        self.open_settings_at(SettingsSection::Models);
        self.models_page_opened()
    }

    /// The Models page lists the cached catalog and asks for a fresh one
    /// when the cache holds none: however the page is reached, a list
    /// with nothing in it never sits there saying nothing.
    pub(crate) fn models_page_opened(&mut self) -> Vec<Effect> {
        if !self.models.is_empty() && !self.catalog_stale {
            return Vec::new();
        }
        // A fetch already out answers for this page: asking again would
        // double the network round trip.
        if let Screen::Settings(settings) = &self.screen
            && settings.model_picker.status == ModelPickerStatus::Loading
        {
            return Vec::new();
        }
        self.ask_models_catalog()
    }

    /// Asks every provider the profile holds for its catalog, the picker
    /// spinning until the answers land. With no provider to ask, the
    /// page says so where the rows would be: the runner would drop the
    /// fetch and the picker would spin on nothing.
    pub(crate) fn ask_models_catalog(&mut self) -> Vec<Effect> {
        // A batch already out answers for this page: asking again would
        // only spend a second round trip on the same providers.
        if self.catalog_inflight {
            if let Screen::Settings(settings) = &mut self.screen
                && settings.section == SettingsSection::Models
                && settings.model_picker.status == ModelPickerStatus::Ready
            {
                settings.model_picker.start_loading();
            }
            return Vec::new();
        }
        let no_candidate =
            Backend::candidates_from_profile(&self.effective(), self.secret_store()).is_empty();
        if let Screen::Settings(settings) = &mut self.screen
            && settings.section == SettingsSection::Models
        {
            if no_candidate {
                settings.model_picker.status = ModelPickerStatus::Error(
                    "no provider on this profile · /login first".to_owned(),
                );
            } else {
                settings.model_picker.start_loading();
            }
        }
        if no_candidate {
            return Vec::new();
        }
        self.catalog_stale = false;
        vec![Effect::FetchModels(FetchWhy::Refresh)]
    }

    /// The Models page's Enter: the highlighted row becomes the
    /// profile's model.
    pub(crate) fn choose_picked_model(&mut self) -> Vec<Effect> {
        let picked = match &self.screen {
            Screen::Settings(settings) => settings.model_picker.selected_row().map(|row| {
                (
                    row.provider().to_owned(),
                    row.instance().map(str::to_owned),
                    row.id().to_owned(),
                )
            }),
            _ => None,
        };
        let Some((provider, instance, id)) = picked else {
            return Vec::new();
        };
        self.choose_settings_model(&provider, instance.as_deref(), &id)
    }

    /// A pick on the Models page: the choice persists as the profile's
    /// model, the agent rebuilds over the conversation, and the page
    /// stays open with its cursor on the new model.
    pub(crate) fn choose_settings_model(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
    ) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; the model pick waits for it to finish");
            return Vec::new();
        }
        match self.apply_model_choice(provider, instance, id) {
            Ok(effects) => {
                // The page reads `stored`; the pick must land there or
                // the Model field would keep the model it replaced.
                let stored = self.effective();
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.stored = stored;
                    // The search resets and the mark and cursor follow
                    // the model just picked.
                    settings.model_picker.filter.clear();
                    settings.sync_current_model();
                }
                self.transcript
                    .push_notice(&format!("model switched to {provider}/{id}"));
                effects
            }
            Err(err) => {
                self.error = Some(err);
                Vec::new()
            }
        }
    }

    /// The system prompt in the transcript, fenced so it reads verbatim.
    pub fn show_system_prompt(&mut self, prompt: &str) {
        self.transcript
            .push_markdown(format!("```markdown\n{prompt}```"));
    }
}
