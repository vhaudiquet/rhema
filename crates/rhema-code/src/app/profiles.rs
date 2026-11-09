//! Profile and sandbox instance management: the switch dance, the
//! settings pages' instance actions, and the sandbox the session
//! rides.

use super::*;

impl App {
    /// Handles one keypress; returns the effects to perform.
    /// Switches the session to a profile: re-resolves the backend from
    /// that profile's settings, rebuilds the agent carrying the session
    /// over, persists the choice, and notices. A profile without usable
    /// credentials is declined in-band.
    /// Opens the profile picker over the config's profiles.
    pub fn open_profile_picker(&mut self) -> Vec<Effect> {
        if self.config.profiles.is_empty() {
            self.push_notice("no profiles defined in the config");
            return Vec::new();
        }
        self.screen = Screen::Profile(crate::screens::ProfileScreen::new(
            &self.config.profiles,
            self.session_profile.as_deref(),
            self.secret_store(),
        ));
        Vec::new()
    }

    /// The kind this session's `bash` currently runs under: the
    /// assembled sandbox's choice, `local` when the agent's own tools
    /// run.
    pub fn active_sandbox(&self) -> SandboxChoice {
        self.sandboxed
            .as_ref()
            .map(|assembled| assembled.choice)
            .unwrap_or(SandboxChoice::Local)
    }

    /// The footer's sandbox badge: the kind `bash` runs under, or the
    /// instance's name when the profile's active pointer names the
    /// instance this kind resolves to. Local reads as a fact too — the
    /// fast path is a choice, not an absence.
    pub fn sandbox_badge(&self) -> String {
        let choice = self.active_sandbox();
        if choice == SandboxChoice::Local {
            return "local".to_owned();
        }
        self.effective()
            .sandbox_instance_for(choice.stored_name())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_else(|| choice.stored_name().to_owned())
    }

    /// `/sandbox <name>`: a configured instance's name runs `bash` in
    /// it — the Sandbox page's enter, which points the section's active
    /// instance at the name; anything else parses as a kind, the
    /// picker's select with the kind spelled out. An unknown name is
    /// declined in-band.
    pub fn choose_sandbox_by_name(&mut self, name: &str) -> Vec<Effect> {
        if self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .contains_key(name)
        {
            return self.activate_sandbox_instance(name);
        }
        match SandboxChoice::from_stored(Some(name)) {
            Some(choice) => self.choose_sandbox(choice),
            None => {
                self.transcript.push_notice(&format!(
                    "no sandbox {name:?}; /sandbox lists this profile's instances and the kinds"
                ));
                Vec::new()
            }
        }
    }

    /// Switches where the session's `bash` runs: the kind persists on
    /// the session's profile's `sandbox` section (the kind only — the
    /// url, host, container, and token keep whatever the config stored),
    /// the workspace remembers it, and the running agent's tools are
    /// rebuilt on the shared assembly. `local` rebuilds inline; every
    /// other kind assembles off the UI thread and answers as
    /// `UiEvent::SandboxAssembled` — until then the current tools keep
    /// running. A kind this machine does not offer is declined in-band.
    pub fn choose_sandbox(&mut self, choice: SandboxChoice) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /sandbox waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; /sandbox waits for it");
            return Vec::new();
        }
        if !self.sandbox_offered.offers(choice) {
            self.transcript.push_notice(&format!(
                "{} is not offered on this machine; /sandbox lists what is",
                choice.stored_name()
            ));
            return Vec::new();
        }
        if self.active_sandbox() == choice {
            self.transcript
                .push_notice(&format!("bash already runs on {}", choice.stored_name()));
            return Vec::new();
        }
        // Kind only: the rest of the section is the config's to keep —
        // the picker does not edit urls, hosts, or tokens.
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let sandbox = profile
                    .sandbox
                    .get_or_insert_with(rhema_config::SandboxSettings::default);
                sandbox.kind = Some(choice.stored_name().to_owned());
            });
        // The workspace remembers the kind, the twin of the profile
        // memory `begin_turn` writes.
        self.config
            .note_sandbox_in(&self.options.workspace, choice.stored_name());
        if !self.save_config() {
            return Vec::new();
        }
        // Enter selects and returns: the picker closes on an accepted
        // choice, the assembly's answer arrives over the chat.
        self.screen = Screen::Chat;
        match choice {
            SandboxChoice::Local => {
                self.replace_sandboxed(None);
                self.rebuild_for_sandbox("bash runs on this host");
                Vec::new()
            }
            non_local => {
                self.assembling = Some(non_local);
                self.transcript.push_notice(&format!(
                    "standing up the {} sandbox…",
                    non_local.stored_name()
                ));
                vec![Effect::AssembleSandbox(non_local)]
            }
        }
    }

    /// Swaps the session's assembled sandbox, stopping the outgoing
    /// one first: a switch to another instance — or back to local — is
    /// an exit for the outgoing session's hold, and its stop-on-exit
    /// answer belongs to this moment, not to the process's end.
    pub(crate) fn replace_sandboxed(&mut self, assembled: Option<crate::sandbox::Sandboxed>) {
        if let Some(outgoing) = self.sandboxed.as_mut() {
            outgoing.shutdown();
        }
        self.sandboxed = assembled;
    }

    /// Rebuilds the running agent carrying the conversation over — the
    /// profile-switch idiom — so the session's sandbox state takes
    /// effect on the tool's executor at once, then notices.
    pub(crate) fn rebuild_for_sandbox(&mut self, notice: &str) {
        // The machine changed: the report re-states where bash runs.
        if let Err(err) = self.rebuild_live_agent(true) {
            self.error = Some(err);
            return;
        }
        self.transcript.push_notice(notice);
    }

    /// Switches the session to a profile: re-resolves the backend from
    /// that profile's settings, rebuilds the agent carrying the session
    /// over, and notices. The choice is per-session, never persisted. A
    /// profile without usable credentials still switches — the session's
    /// identity is what moves — but the old provider goes with the old
    /// profile: turns wait for /login rather than leaking the previous
    /// profile's key.
    pub fn switch_profile(&mut self, name: &str) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /profile waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; /profile waits for it");
            return Vec::new();
        }
        if self.session_profile.as_deref() == Some(name) {
            self.transcript
                .push_notice(&format!("already on profile {name}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(name));
        self.options.thinking = effective.thinking_effort();
        // Profiles are isolated, `bash` included: the offered set
        // re-derives for the profile moving in, and where its own
        // `sandbox` section says `bash` runs is where this session's
        // `bash` goes — local when the profile names nothing, never a
        // sandbox the profile just left behind.
        self.sandbox_offered = self.sandbox_machines.offered_for(&effective);
        let wanted = profile_sandbox(&effective, &self.sandbox_offered);
        let keep = match self.sandboxed.as_ref() {
            None => wanted == SandboxChoice::Local,
            Some(current) => {
                current.choice == wanted
                    && wanted != SandboxChoice::Local
                    && Some(current.target.as_str())
                        == crate::sandbox::sandbox_target(&effective, wanted.stored_name())
                            .as_deref()
            }
        };
        if !keep && wanted == SandboxChoice::Local {
            self.replace_sandboxed(None);
        }
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let Some(mut backend) = Backend::from_profile(&effective, self.secret_store()) else {
            // Never recorded: startup re-validates usability, so the
            // memory naming a keyless profile would only fall through.
            // The agent drops with the provider: a turn here would run
            // on the profile just left. The sandbox leaves with the
            // profile too — nothing keyless can run a turn in it.
            if !keep {
                self.replace_sandboxed(None);
            }
            self.session_profile = Some(name.to_owned());
            self.backend = None;
            self.provider = None;
            self.agent = None;
            self.models.clear();
            self.session = session;
            self.screen = Screen::Chat;
            self.transcript.push_notice(&format!(
                "switched to profile {name}; no API key, /login to add one"
            ));
            return Vec::new();
        };
        // A switch keeps the conversation: Console sees the same
        // OpenCode session id across profiles when the kind matches.
        if let Some(previous) = &self.backend {
            backend.keep_session(previous);
        }
        let (agent_config, subagents) = session_agent_config(
            &self.options,
            self.sandboxed.as_ref(),
            effective.model.clone(),
            backend.provider(),
            &self.config.skills_disabled.clone(),
            &self.config.disabled_subagents,
            &effective,
            self.session_profile
                .as_deref()
                .unwrap_or(rhema_config::DEFAULT_PROFILE),
            &self.config.disabled_mcp_servers,
            self.secrets.as_ref(),
        );
        match CodingAgent::from_session(backend.provider(), agent_config, session) {
            Ok(mut agent) => {
                // The benchmark moves with the conversation, the way
                // the cost does: the old agent's counters fold into
                // the fresh mirror.
                let saved = self
                    .agent
                    .as_ref()
                    .and_then(CodingAgent::transcript_stats)
                    .or(self.transcript_stats);
                let sections = self
                    .agent
                    .as_ref()
                    .map(CodingAgent::transcript_sections)
                    .unwrap_or_default();
                if let Some(saved) = saved {
                    agent.carry_transcript_stats(&saved);
                }
                agent.adopt_transcript_sections(sections);
                self.session_profile = Some(name.to_owned());
                self.backend = Some(backend.clone());
                self.provider = Some(backend.provider());
                self.models.clear();
                self.agent = Some(agent);
                self.subagents = Some(subagents);
                self.screen = Screen::Chat;
                // An explicit switch is a choice: recorded for this
                // workspace now, not at the next turn.
                self.remember_profile();
                let mut notice = format!("switched to profile {name}");
                if effective.model.is_none() {
                    notice.push_str("; /model to pick a model");
                }
                let mut effects = vec![Effect::FetchUsage];
                if keep {
                    // The profile moving in resolves to the sandbox
                    // already running: nothing moves.
                } else if wanted == SandboxChoice::Local {
                    notice.push_str("; bash runs on this host");
                } else {
                    // The old tools keep running until the assembly
                    // lands (the picker's posture): the answer arrives
                    // as `UiEvent::SandboxAssembled`, which swaps the
                    // tools and rebuilds again.
                    self.assembling = Some(wanted);
                    notice.push_str(&format!(
                        "; standing up the {} sandbox…",
                        wanted.stored_name()
                    ));
                    effects.push(Effect::AssembleSandbox(wanted));
                }
                self.transcript.push_notice(&notice);
                effects
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
            }
        }
    }

    /// Refills the settings Profiles page's snapshot from the config:
    /// the registry the add, remove, and color edits just changed.
    pub(crate) fn sync_profile_rows(&mut self) {
        let rows = crate::screens::profile_rows(
            &self.config.profiles,
            self.session_profile.as_deref(),
            self.secret_store(),
        );
        if let Screen::Settings(settings) = &mut self.screen {
            settings.profiles = rows;
        }
    }

    /// The add-profile dialog's accepted name: a fresh, empty profile
    /// joins the registry — provider, model, and key stay unset, the
    /// Providers page's login flow fills those in — and the snapshot
    /// follows. The session does not move: creation is not a switch.
    pub(crate) fn create_profile(&mut self) -> Vec<Effect> {
        let name = match &self.screen {
            Screen::Settings(settings) => match &settings.add_profile {
                Some(dialog) => dialog.name().to_owned(),
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        if name.is_empty() {
            return Vec::new();
        }
        if self.config.profiles.contains_key(&name) {
            // The dialog dedupes against its opening snapshot; this is
            // the belt for a path that should not exist.
            self.transcript
                .push_notice(&format!("a profile named {name} already exists"));
            return Vec::new();
        }
        self.config
            .profiles
            .insert(name.clone(), rhema_config::Profile::default());
        if !self.save_config() {
            return Vec::new();
        }
        if let Screen::Settings(settings) = &mut self.screen {
            settings.add_profile = None;
        }
        self.sync_profile_rows();
        self.transcript
            .push_notice(&format!("added profile {name}"));
        Vec::new()
    }

    /// The add-sandbox dialog's accepted kind and value: the kind's
    /// field lands on the session profile's `sandbox` section — the
    /// container, the host, or the url — and the snapshot follows.
    /// Storing never activates: enter on a kind row runs `bash` there.
    pub(crate) fn submit_sandbox_instance(&mut self) -> Vec<Effect> {
        let kind = match &self.screen {
            Screen::Settings(settings) => match &settings.sandbox_dialog {
                Some(dialog) => dialog.picked(),
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        // The instance carries no target: the concrete name is the
        // workspace's to set, derived at assembly time and stable
        // across sessions. The row's name is the kind's — first free,
        // `docker-2` once `docker` is taken. Storing never activates.
        let mut name = kind.stored_name().to_owned();
        let mut suffix = 2;
        while self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .contains_key(&name)
        {
            name = format!("{}-{suffix}", kind.stored_name());
            suffix += 1;
        }
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                profile.place_sandbox_instance(
                    name.clone(),
                    rhema_config::SandboxInstance {
                        kind: kind.stored_name().to_owned(),
                        target: None,
                        vm: None,
                    },
                );
            });
        if !self.save_config() {
            return Vec::new();
        }
        if let Screen::Settings(settings) = &mut self.screen {
            settings.sandbox_dialog = None;
            settings.stored = self.config.effective(self.session_profile.as_deref());
        }
        self.transcript.push_notice(&format!(
            "the {name} instance is stored; enter its row to run bash there"
        ));
        Vec::new()
    }

    /// `d` on a Sandbox page instance row: remove the instance. The
    /// record goes from the map and the section's active pointer goes
    /// with it; a session already running there keeps running, the
    /// providers' stance. The next assembly under the stored kind
    /// falls through to the section's facts or a derived name.
    pub(crate) fn remove_sandbox_instance(&mut self) -> Vec<Effect> {
        let name = match &self.screen {
            Screen::Settings(settings) => settings.selected_sandbox_instance().map(str::to_owned),
            _ => return Vec::new(),
        };
        let Some(name) = name else {
            return Vec::new();
        };
        let mut removed = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                removed = profile.remove_sandbox_instance(&name);
            });
        if removed.is_none() {
            return Vec::new();
        }
        if !self.save_config() {
            return Vec::new();
        }
        self.transcript
            .push_notice(&format!("the {name} sandbox instance is removed"));
        if let Screen::Settings(settings) = &mut self.screen {
            settings.stored = self.config.effective(self.session_profile.as_deref());
        }
        Vec::new()
    }

    /// Enter on a Sandbox page instance row: run `bash` in it — the
    /// same flow `/sandbox <kind>` runs, with the section's active
    /// pointer naming the instance so the assembly reads its target.
    /// The session's kind moves only when the instance's kind differs;
    /// two instances of one kind switch without touching the kind.
    pub(crate) fn activate_sandbox_instance(&mut self, name: &str) -> Vec<Effect> {
        let instance = self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .get(name)
            .cloned();
        let Some(instance) = instance else {
            return Vec::new();
        };
        let Some(choice) = crate::args::SandboxChoice::from_stored(Some(&instance.kind)) else {
            self.transcript.push_notice(&format!(
                "the {name} instance's kind is not one rhema runs: {}",
                instance.kind
            ));
            return Vec::new();
        };
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; the sandbox waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; the sandbox waits for it");
            return Vec::new();
        }
        if !self.sandbox_offered.offers(choice) {
            self.transcript.push_notice(&format!(
                "{} is not offered on this machine; the {name} instance cannot run here",
                choice.stored_name()
            ));
            return Vec::new();
        }
        let active_name = self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.active.clone());
        if active_name.as_deref() == Some(name) && self.active_sandbox() == choice {
            self.transcript
                .push_notice(&format!("bash already runs in the {name} instance"));
            return Vec::new();
        }
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let sandbox = profile
                    .sandbox
                    .get_or_insert_with(rhema_config::SandboxSettings::default);
                sandbox.kind = Some(choice.stored_name().to_owned());
                sandbox.active = Some(name.to_owned());
            });
        self.config
            .note_sandbox_in(&self.options.workspace, choice.stored_name());
        if !self.save_config() {
            return Vec::new();
        }
        // Enter selects and returns: the page closes on an accepted
        // choice, the assembly's answer arrives over the chat.
        self.screen = Screen::Chat;
        match choice {
            SandboxChoice::Local => {
                self.replace_sandboxed(None);
                self.rebuild_for_sandbox("bash runs on this host");
                Vec::new()
            }
            non_local => {
                self.assembling = Some(non_local);
                self.transcript
                    .push_notice(&format!("standing up the {name} sandbox instance…"));
                vec![Effect::AssembleSandbox(non_local)]
            }
        }
    }

    /// `d` on a Profiles page row: forget the profile. The one this
    /// session runs on stays — the session's identity is not dropped
    /// from under it, and that refusal is also what keeps the registry
    /// from emptying. The workspace memory forgets the name with the
    /// registry, the way a rename does.
    pub(crate) fn remove_profile(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_profile().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        if self.session_profile.as_deref() == Some(name.as_str()) {
            self.transcript
                .push_notice("the profile this session runs on cannot be removed — switch first");
            return Vec::new();
        }
        self.config.profiles.remove(&name);
        self.config
            .last_profile_by_workspace
            .retain(|_, remembered| remembered != &name);
        if !self.save_config() {
            return Vec::new();
        }
        self.sync_profile_rows();
        // Keep the selection inside the shrunken list.
        if let Screen::Settings(settings) = &mut self.screen {
            let rows = settings.profiles.len() + 1;
            if settings.row >= rows {
                settings.row = rows.saturating_sub(1);
            }
        }
        self.transcript
            .push_notice(&format!("removed profile {name}"));
        Vec::new()
    }

    /// `c` on a Profiles page row: the name color moves to the next
    /// entry of the default wheel — the theme's auto first, then each
    /// default hex — and persists. The page's swatch is the record;
    /// no notice.
    pub(crate) fn cycle_profile_color(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_profile().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let stored = self
            .config
            .profiles
            .get(&name)
            .and_then(|profile| profile.color.as_deref());
        let next = Self::next_profile_color(stored);
        self.config.set_for_profile(Some(&name), |profile| {
            profile.color = next.map(str::to_owned)
        });
        if !self.save_config() {
            return Vec::new();
        }
        self.sync_profile_rows();
        Vec::new()
    }

    /// The next entry of the profile color wheel after `stored`: auto
    /// (no stored color) first, then each default hex, wrapping to
    /// auto. A hand-edited color the wheel does not spell rejoins at
    /// auto.
    pub(crate) fn next_profile_color(stored: Option<&str>) -> Option<&'static str> {
        match stored {
            None => Some(crate::palette::PROFILE_COLORS[0]),
            Some(hex) => crate::palette::PROFILE_COLORS
                .iter()
                .position(|color| color.eq_ignore_ascii_case(hex))
                .and_then(|index| crate::palette::PROFILE_COLORS.get(index + 1))
                .copied(),
        }
    }

    /// Enter on a logged-in instance: it becomes the profile's active
    /// one, and the agent rebuilds carrying the conversation over — a
    /// picker when no model is stored yet, the chat otherwise.
    pub(crate) fn activate_instance(&mut self) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running — the switch waits for it to finish");
            return Vec::new();
        }
        let tab = match &self.screen {
            Screen::Settings(settings) => settings.profile.clone(),
            _ => return Vec::new(),
        };
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_instance().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let Some(instance) = self
            .config
            .profiles
            .get(&tab)
            .and_then(|profile| profile.instances.get(&name).cloned())
        else {
            return Vec::new();
        };
        // A stored model id only carries over when the provider stayed
        // the same: gpt-x means nothing to a llama.cpp server.
        let previous_provider = self.config.effective(Some(&tab)).provider;
        let keep_model = previous_provider.as_deref() == Some(instance.provider.as_str());
        self.config.set_for_profile(Some(&tab), |settings| {
            settings.provider = Some(instance.provider.clone());
            settings.instance = Some(name.clone());
            if !keep_model {
                settings.model = None;
            }
        });
        if !self.save_config() {
            return Vec::new();
        }
        let effective = self.config.effective(Some(&tab));
        self.options.thinking = effective.thinking_effort();
        let Some(mut backend) = Backend::from_profile(&effective, self.secret_store()) else {
            self.error = Some(format!(
                "no usable credentials for {} — /login first",
                instance.provider
            ));
            return Vec::new();
        };
        if let Some(previous) = &self.backend {
            backend.keep_session(previous);
        }
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let label = format!("{} — {name}", backend.config_name());
        // The agent builds on the new provider, so it installs first.
        self.provider = Some(backend.provider());
        self.backend = Some(backend);
        let (saved, sections) = self.outgoing_transcript();
        let saved = saved.or(self.transcript_stats);
        // The model may have changed with the profile, and the cache is
        // per-model regardless: the report re-states its facts.
        let mut session = session;
        rhema_coding_agent::clear_environment_stamp(&mut session);
        if let Err(err) = self.rebuild_agent(session, saved, sections) {
            self.error = Some(err);
            return Vec::new();
        }
        self.session_profile = Some(tab);
        if let Some(model) = effective.model {
            self.screen = Screen::Chat;
            self.transcript
                .push_notice(&format!("switched to {label} · {model}"));
            // The session now rides the instance it activated:
            // that plan's numbers are the badge's to fetch.
            vec![Effect::FetchUsage]
        } else {
            self.screen = Screen::Picker(PickerScreen {
                status: PickerStatus::Loading,
                ..PickerScreen::new(self.models.clone())
            });
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        }
    }

    /// `d` on a logged-in instance: forget it. When it was the active
    /// one, the backend re-resolves from what remains — another instance
    /// of the provider, or none.
    pub(crate) fn remove_instance(&mut self) -> Vec<Effect> {
        let tab = match &self.screen {
            Screen::Settings(settings) => settings.profile.clone(),
            _ => return Vec::new(),
        };
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_instance().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        // The keychain entry goes with the instance: a removed login
        // must not leave its key in the chain. Best effort — a store
        // that refuses leaves the entry; it holds no live reference.
        let mut removed = None;
        self.config.set_for_profile(Some(&tab), |settings| {
            removed = settings.remove_instance(&name);
        });
        // The catalog drops the instance's slice with it: nothing
        // re-fetches a removed login, and the next fetch's merge keeps
        // unqueried slices, so a stale copy would live forever.
        if let Some(old) = &removed {
            self.models.retain(|entry| {
                entry.provider != old.provider || entry.instance.as_deref() != Some(name.as_str())
            });
        }
        if let Some(id) = removed.and_then(|old| old.secret_ref)
            && let Some(store) = self.secret_store()
        {
            // The login leaves the config either way; a keychain
            // that refuses the delete leaves the secret behind, and
            // that must not pass silently.
            if let Err(err) = store.delete(&id) {
                self.error = Some(format!(
                    "the login is removed, but its keychain entry could not be deleted: {err}"
                ));
            }
        }
        if !self.save_config() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if self.session_profile.as_deref() == Some(tab.as_str()) {
            let effective = self.config.effective(Some(&tab));
            match Backend::from_profile(&effective, self.secret_store()) {
                Some(mut backend) => {
                    if let Some(previous) = &self.backend {
                        backend.keep_session(previous);
                    }
                    self.backend = Some(backend.clone());
                    self.provider = Some(backend.provider());
                    // What remains is a different plan: refresh the
                    // badge against it.
                    effects.push(Effect::FetchUsage);
                }
                None => {
                    // The agent drops with the provider, and its
                    // session comes home before the drop: the live
                    // history lands in `self.session`, so the exit
                    // save keeps this checkpoint instead of clobbering
                    // it with the stale copy held before the agent
                    // existed.
                    if let Some(agent) = self.agent.take() {
                        self.session = agent.session().clone();
                    }
                    self.backend = None;
                    self.provider = None;
                }
            }
        }
        // Sync the screen's copy of the profile and keep the selection
        // inside the shrunken list.
        if let Screen::Settings(settings) = &mut self.screen {
            settings.stored = self.config.effective(Some(&tab));
            let rows = settings.instance_rows().len();
            if settings.row >= rows {
                settings.row = rows.saturating_sub(1);
            }
        }
        self.transcript.push_notice(&format!("forgot {name}"));
        effects
    }

    /// Records the session's profile as the last one this workspace
    /// chose or ran with — an explicit `/profile` switch or a turn's
    /// start — so the next launch here opens on it. Written only on
    /// change; a failure surfaces in the status strip like every other
    /// config write — neither the switch nor the turn is blocked by it.
    pub(crate) fn remember_profile(&mut self) {
        let Some(profile) = self.session_profile.clone() else {
            return;
        };
        let workspace = self.options.workspace.clone();
        if self.config.last_profile_in(&workspace).as_deref() == Some(profile.as_str()) {
            return;
        }
        self.config.note_profile_in(&workspace, &profile);
        self.save_config();
    }
}
