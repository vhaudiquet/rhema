//! Model choice and login: the pickers, the catalog merge, the
//! credential installs, and the agent rebuild the changes need.

use super::*;

impl App {
    /// Persists the config through the store. `false` means the store
    /// refused and the error strip carries why — callers that must not
    /// pretend success stop there.
    pub(crate) fn save_config(&mut self) -> bool {
        match self.store.save(&self.config) {
            Ok(()) => true,
            Err(err) => {
                self.error = Some(format!("cannot save config: {err}"));
                false
            }
        }
    }

    /// Sets the session's thinking effort: persisted into the session's
    /// profile like a model pick, and the agent rebuilds carrying the
    /// conversation so the very next turn asks at the new level.
    pub fn set_thinking(&mut self, effort: rhema_core::request::ThinkingEffort) {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /thinking waits for it to finish");
            return;
        }
        self.options.thinking = effort;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |settings| {
                settings.thinking = Some(effort.as_str().to_owned());
            });
        if !self.save_config() {
            return;
        }
        if let Err(err) = self.rebuild_live_agent(false) {
            self.error = Some(err);
            return;
        }
        self.transcript
            .push_notice(&format!("thinking effort set to {}", effort.as_str()));
    }

    /// The add-instance dialog validated a new instance's input: fetch
    /// the models; the fetch is the validation (a wrong key or a dead
    /// server both surface as the fetch's error). The dialog lives on
    /// the settings screen or the onboarding wizard — both run the same
    /// login flow.
    pub(crate) fn validate_login(&mut self) -> Vec<Effect> {
        let pending_input = match &self.screen {
            Screen::Settings(settings) => settings
                .dialog
                .as_ref()
                .map(|dialog| (dialog.pending.clone(), dialog.input.trim().to_owned())),
            Screen::Onboarding(wizard) => wizard
                .dialog
                .as_ref()
                .map(|dialog| (dialog.pending.clone(), dialog.input.trim().to_owned())),
            _ => None,
        };
        let Some((Some(pending), input)) = pending_input else {
            return Vec::new();
        };
        let base_url = pending
            .base_url
            .clone()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());
        let backend = match pending.provider {
            crate::screens::LoginProvider::LlamaCpp => Backend::LlamaCpp {
                // The URL prompt was captured earlier; the current line
                // is the bearer key, optional on an open server.
                url: base_url.unwrap_or_default(),
                api_key: (!input.is_empty()).then_some(input),
            },
            crate::screens::LoginProvider::OpenAi => Backend::OpenAi {
                api_key: input,
                base_url,
            },
            crate::screens::LoginProvider::OpenRouter => Backend::OpenRouter { api_key: input },
            crate::screens::LoginProvider::Anthropic => Backend::Anthropic { api_key: input },
            crate::screens::LoginProvider::DeepSeek => Backend::DeepSeek { api_key: input },
            crate::screens::LoginProvider::Zai => Backend::Zai {
                api_key: input,
                base_url,
            },
            // The flow path never validates a typed key: its credential
            // arrives from the flow task, already wrapped in the backend.
            crate::screens::LoginProvider::Copilot => return Vec::new(),
            crate::screens::LoginProvider::Google => Backend::Google { api_key: input },
        };
        vec![Effect::FetchModels(FetchWhy::Login {
            backend,
            instance: Some(pending.name),
        })]
    }

    /// A step of the running login flow: render it on the add-instance
    /// dialog — the settings' modal or the wizard's inline copy — and
    /// on completion validate the credential it produced through the
    /// same fetch path a typed key takes.
    pub(crate) fn on_login_update(&mut self, update: rhema_provider::LoginUpdate) -> Vec<Effect> {
        let dialog = match &mut self.screen {
            Screen::Settings(settings) => settings.dialog.as_mut(),
            Screen::Onboarding(wizard) => wizard.dialog.as_mut(),
            // The flow was left behind (Esc raced a completed update):
            // drop the credential with the screen it belonged to.
            _ => {
                self.login_cancel = None;
                return Vec::new();
            }
        };
        let Some(dialog) = dialog else {
            self.login_cancel = None;
            return Vec::new();
        };
        match update {
            rhema_provider::LoginUpdate::Instruct(instruction) => {
                dialog.flow_instruction = Some(instruction);
                dialog.flow_waiting = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Waiting(text) => {
                dialog.flow_waiting = Some(text);
                Vec::new()
            }
            rhema_provider::LoginUpdate::Failed(message) => {
                dialog.status = crate::screens::DialogStatus::Error(message);
                // The flow task has ended; the handle can go.
                self.login_cancel = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Completed(token) => {
                dialog.flow_waiting = Some("fetching the model list".to_owned());
                self.login_cancel = None;
                // The flow's instance was named before the flow started.
                let instance = dialog.pending.as_ref().map(|p| p.name.clone());
                vec![Effect::FetchModels(FetchWhy::Login {
                    backend: Backend::Copilot { api_key: token },
                    instance,
                })]
            }
        }
    }

    /// Persists a validated login: the instance lands in `tab`'s profile
    /// (its key in the keychain when the store takes it, the file when
    /// it does not; a re-login's old keychain entry goes first), the
    /// session rides the validated backend, and the catalog's slice for
    /// the instance replaces itself. The error, if any, is for the
    /// dialog to render inline.
    pub(crate) fn install_login(
        &mut self,
        tab: &str,
        instance: &str,
        backend: &Backend,
        models: &[ModelCard],
    ) -> Result<(), String> {
        let provider = backend.config_name();
        let (api_key, base_url) = match backend {
            Backend::LlamaCpp { url, api_key } => (api_key.clone(), Some(url.clone())),
            Backend::OpenAi { api_key, base_url } | Backend::Zai { api_key, base_url } => {
                (Some(api_key.clone()), base_url.clone())
            }
            Backend::Google { api_key }
            | Backend::OpenRouter { api_key }
            | Backend::Anthropic { api_key }
            | Backend::DeepSeek { api_key }
            | Backend::Copilot { api_key } => (Some(api_key.clone()), None),
        };
        let secrets = self.secrets.as_deref();
        let mut keychain_delete_failure: Option<String> = None;
        self.config.set_for_profile(Some(tab), |settings| {
            // A re-login replaces the instance wholesale; its old
            // keychain entry must not linger — but a store that refuses
            // the delete must not pass silently either, or the secret
            // outlives the login that named it.
            if let Some(old) = settings.instances.get(instance)
                && let Some(id) = old.secret_ref.as_deref()
                && let Some(store) = secrets
                && let Err(err) = store.delete(id)
            {
                keychain_delete_failure = Some(err.to_string());
            }
            settings.provider = Some(provider.to_owned());
            settings.instance = Some(instance.to_owned());
            settings.upsert_instance(
                instance,
                rhema_config::ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: None,
                    secret_ref: None,
                    base_url,
                },
            );
            if let Some(key) = api_key.as_deref() {
                settings.place_key(instance, key, secrets);
            }
        });
        if let Some(err) = keychain_delete_failure {
            self.error = Some(format!(
                "re-logged in, but the previous keychain entry could not be deleted: {err}"
            ));
        }
        self.store
            .save(&self.config)
            .map_err(|err| format!("cannot save config: {err}"))?;
        self.session_profile = Some(tab.to_owned());
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        // The other logins' catalogs stay: a fresh login must not blank
        // the picker down to one provider. The instance's own slice
        // replaces itself, so a stale copy cannot linger — a provider
        // may hold several.
        self.models.retain(|entry| {
            entry.provider != provider || entry.instance.as_deref() != Some(instance)
        });
        self.models
            .extend(models.iter().map(|model| crate::screens::CatalogModel {
                provider,
                instance: Some(instance.to_owned()),
                model: model.clone(),
            }));
        self.models.sort_by(|a, b| {
            a.provider
                .cmp(b.provider)
                .then(a.instance.cmp(&b.instance))
                .then(a.model.id.cmp(&b.model.id))
        });
        Ok(())
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    pub(crate) fn on_models_loaded(
        &mut self,
        why: FetchWhy,
        result: Vec<crate::events::CatalogFetch>,
    ) -> Vec<Effect> {
        match why {
            FetchWhy::Login { backend, instance } => {
                // Every login writes a named instance; a lost name (a
                // flow that outlived its screen) falls back to the
                // provider's own.
                let instance = instance.unwrap_or_else(|| backend.config_name().to_owned());
                // The login fetch validates one provider: its result is
                // the catalog's only entry.
                match result.into_iter().next().map(|entry| entry.result) {
                    Some(Ok(models)) => {
                        let provider = backend.config_name();
                        // Both login hosts edit the session's profile:
                        // the settings' Providers page and the wizard's
                        // step are only different faces of it.
                        let tab = match &self.screen {
                            Screen::Settings(settings) => settings.profile.clone(),
                            Screen::Onboarding(_) => self
                                .session_profile
                                .clone()
                                .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned()),
                            // The dialog was left behind (Esc raced a
                            // completed fetch): the login has nowhere
                            // to land.
                            _ => return Vec::new(),
                        };
                        if let Err(err) = self.install_login(&tab, &instance, &backend, &models) {
                            match &mut self.screen {
                                Screen::Settings(settings) => {
                                    if let Some(dialog) = &mut settings.dialog {
                                        dialog.status =
                                            crate::screens::DialogStatus::Error(err.clone());
                                    }
                                }
                                Screen::Onboarding(wizard) => {
                                    if let Some(dialog) = &mut wizard.dialog {
                                        dialog.status =
                                            crate::screens::DialogStatus::Error(err.clone());
                                    }
                                }
                                _ => {}
                            }
                            return Vec::new();
                        }
                        // The dialog served its purpose; the wizard's
                        // copy goes too (the model step renders from its
                        // own rows).
                        if let Screen::Settings(settings) = &mut self.screen {
                            settings.dialog = None;
                        }
                        // Where the login lands depends on the screen it
                        // came from: the settings flow opens the picker
                        // (skipping straight to the chat on a
                        // single-model server), the wizard advances to
                        // its own model step (or takes the single model
                        // straight to the profile step).
                        if let Screen::Onboarding(wizard) = &mut self.screen {
                            wizard.backend_ready = true;
                            wizard.stored = self.config.effective(Some(&tab));
                            // The dialog served its purpose; its keys
                            // must not eat the model step's.
                            wizard.dialog = None;
                        }
                        if matches!(self.screen, Screen::Onboarding(_)) {
                            if models.len() == 1 {
                                let id = models[0].id.clone();
                                return match self.apply_model_choice(provider, Some(&instance), &id)
                                {
                                    Ok(effects) => {
                                        if let Screen::Onboarding(wizard) = &mut self.screen {
                                            wizard.chosen_model = Some(id);
                                            wizard.chosen_provider = Some(provider.to_owned());
                                            wizard.go(crate::onboarding::OnboardingStep::Profile);
                                        }
                                        effects
                                    }
                                    Err(err) => {
                                        if let Screen::Onboarding(wizard) = &mut self.screen {
                                            wizard.error = Some(err);
                                        }
                                        Vec::new()
                                    }
                                };
                            }
                            let rows: Vec<crate::screens::CatalogModel> = self
                                .models
                                .iter()
                                .filter(|entry| {
                                    entry.provider == provider
                                        && entry.instance.as_deref() == Some(instance.as_str())
                                })
                                .cloned()
                                .collect();
                            if let Screen::Onboarding(wizard) = &mut self.screen {
                                wizard.models = rows;
                                wizard.go(crate::onboarding::OnboardingStep::Model);
                            }
                            return vec![Effect::FetchUsage];
                        }
                        // A single-model server (plain llama-server rather
                        // than a router) has nothing to pick: take it and
                        // go straight to chat.
                        if models.len() == 1 {
                            let id = models[0].id.clone();
                            return self.choose_model(provider, Some(&instance), &id);
                        }
                        self.screen = Screen::Picker(PickerScreen::new(self.models.clone()));
                        // A login installs the backend it validated: that
                        // plan is the session's numbers now.
                        vec![Effect::FetchUsage]
                    }
                    Some(Err(message)) => {
                        match &mut self.screen {
                            Screen::Settings(settings) => {
                                if let Some(dialog) = &mut settings.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            Screen::Onboarding(wizard) => {
                                if let Some(dialog) = &mut wizard.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                    // The dispatch always spawns one backend per login;
                    // an empty result cannot arrive.
                    None => {
                        let message = "no provider answered".to_owned();
                        match &mut self.screen {
                            Screen::Settings(settings) => {
                                if let Some(dialog) = &mut settings.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            Screen::Onboarding(wizard) => {
                                if let Some(dialog) = &mut wizard.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                }
            }
            FetchWhy::Bootstrap | FetchWhy::Refresh => {
                // One result per queried backend: each fetched slice
                // replaces itself in the catalog, and a slice no fetch
                // asked about — a login that raced this one — survives. A
                // total failure is the picker's error, and a partial one
                // is named in its footer — a missing provider must never
                // look like an empty one.
                let (catalogs, failed): (Vec<_>, Vec<_>) =
                    result.into_iter().partition(|entry| entry.result.is_ok());
                for entry in catalogs.iter().chain(failed.iter()) {
                    self.models.retain(|existing| {
                        existing.provider != entry.provider
                            || existing.instance.as_deref() != entry.instance.as_deref()
                    });
                }
                self.models.extend(catalogs.iter().flat_map(|entry| {
                    let models = entry.result.as_deref().unwrap_or_default();
                    models
                        .iter()
                        .map(move |model| crate::screens::CatalogModel {
                            provider: entry.provider,
                            instance: entry.instance.clone(),
                            model: model.clone(),
                        })
                }));
                self.models.sort_by(|a, b| {
                    a.provider
                        .cmp(b.provider)
                        .then(a.instance.cmp(&b.instance))
                        .then(a.model.id.cmp(&b.model.id))
                });
                if let Screen::Picker(picker) = &mut self.screen {
                    if catalogs.is_empty() {
                        let message = failed
                            .into_iter()
                            .next()
                            .and_then(|entry| entry.result.err())
                            .unwrap_or_else(|| "no provider answered".to_owned());
                        picker.status = PickerStatus::Error(message);
                    } else {
                        picker.set_models(self.models.clone());
                        picker.failed = failed.into_iter().map(|entry| entry.provider).collect();
                        picker.status = PickerStatus::Ready;
                    }
                }
                Vec::new()
            }
        }
    }

    /// A model was named for `/model <needle>`: resolve the needle against
    /// the cached catalog — exact id first, then a unique prefix — and
    /// switch, or say what went missing. `/model` with no needle opens
    /// the full picker instead.
    pub fn choose_model_by_id(&mut self, needle: &str) -> Vec<Effect> {
        // The active instance wins ties: the same model id may be served
        // by several instances of one provider.
        let active = self.effective().instance;
        let mut tag: Vec<(&'static str, Option<String>, String)> = self
            .models
            .iter()
            .filter(|entry| entry.model.id == needle)
            .map(|entry| {
                (
                    entry.provider,
                    entry.instance.clone(),
                    entry.model.id.clone(),
                )
            })
            .collect();
        let prefix: Vec<(&'static str, Option<String>, String)> = self
            .models
            .iter()
            .filter(|entry| entry.model.id.starts_with(needle))
            .map(|entry| {
                (
                    entry.provider,
                    entry.instance.clone(),
                    entry.model.id.clone(),
                )
            })
            .collect();
        let (exact_count, prefix_count) = (tag.len(), prefix.len());
        let resolved = if exact_count == 1 {
            tag.pop()
        } else if exact_count > 1 {
            tag.into_iter().find(|(_, instance, _)| *instance == active)
        } else if prefix_count == 1 {
            prefix.into_iter().next()
        } else if prefix_count > 1 {
            prefix
                .into_iter()
                .find(|(_, instance, _)| *instance == active)
        } else {
            None
        };
        match resolved {
            Some((provider, instance, id)) => self.choose_model(provider, instance.as_deref(), &id),
            None if exact_count > 1 || prefix_count > 1 => {
                self.push_notice(&format!(
                    "{} models match {needle:?}; name one fully or use /model",
                    exact_count.max(prefix_count)
                ));
                Vec::new()
            }
            None => {
                self.push_notice(&format!(
                    "no model matches {needle:?} in the cached catalog; /model lists them"
                ));
                Vec::new()
            }
        }
    }

    /// A model was chosen in the picker: persist it and (re)build the
    /// agent, carrying the current session over, and land in the chat.
    /// The old agent is only dropped once the new one built, so a
    /// rebuild failure cannot strand the chat.
    pub(crate) fn choose_model(
        &mut self,
        provider: &'static str,
        instance: Option<&str>,
        id: &str,
    ) -> Vec<Effect> {
        let from_chat =
            matches!(&self.screen, Screen::Picker(_)) || matches!(&self.screen, Screen::Chat);
        match self.apply_model_choice(provider, instance, id) {
            Ok(effects) => {
                // An interactive switch — the picker opened from the
                // chat, or a `/model <needle>` typed there — says where
                // it landed; the login auto-selects stay quiet.
                if from_chat {
                    self.transcript
                        .push_notice(&format!("model switched to {provider}/{id}"));
                }
                self.screen = Screen::Chat;
                effects
            }
            Err(err) => {
                self.error = Some(err);
                Vec::new()
            }
        }
    }

    /// The model choice without the screen move: the pick persists into
    /// the session's profile, the backend re-resolves, and the agent
    /// rebuilds carrying the conversation over. The chat's picker path
    /// and the onboarding wizard's step share it; each lands where it
    /// needs to afterwards.
    pub(crate) fn apply_model_choice(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
    ) -> Result<Vec<Effect>, String> {
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        // The write follows the session's profile, not the startup flag:
        // after a mid-session /profile the pick must land where effective()
        // reads it back from — profiles never inherit each other's model.
        // The provider — and the instance it came from — moves with the
        // model: a pick from another login's catalog switches the backend
        // this session rides on.
        let profile = self.session_profile.as_deref();
        self.config.set_for_profile(profile, |settings| {
            settings.provider = Some(provider.to_owned());
            settings.instance = instance.map(str::to_owned);
            settings.model = Some(id.to_owned());
        });
        if let Err(err) = self.store.save(&self.config) {
            return Err(format!("cannot save config: {err}"));
        }
        let backend = match Backend::from_profile(&self.effective(), self.secret_store()) {
            Some(backend) => backend,
            None => {
                return Err(format!(
                    "no usable credentials for {provider}; /login first"
                ));
            }
        };
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        let (saved, sections) = self.outgoing_transcript();
        let saved = saved.or(self.transcript_stats);
        // The model may have changed, and the cache is per-model
        // regardless: the report re-states its facts.
        let mut session = session;
        rhema_coding_agent::clear_environment_stamp(&mut session);
        self.rebuild_agent(session, saved, sections)?;
        // A new backend counts a new plan: the allowance badge fetches
        // now, not at the first turn's end.
        Ok(vec![Effect::FetchUsage])
    }

    /// The outgoing agent's counters and sections, read before a
    /// rebuild: the pair [`Self::rebuild_agent`] carries onto the new
    /// agent, so the settings pages keep reading as one run.
    pub(crate) fn outgoing_transcript(
        &self,
    ) -> (
        Option<rhema_transcript::TranscriptStats>,
        Vec<rhema_transcript::Section>,
    ) {
        (
            self.agent.as_ref().and_then(CodingAgent::transcript_stats),
            self.agent
                .as_ref()
                .map(CodingAgent::transcript_sections)
                .unwrap_or_default(),
        )
    }

    /// Swaps the session's agent for one built over `session` under
    /// the current profile and flags — the rebuild idiom. The
    /// transcript counters and sections given here ride along: same
    /// conversation, new executor. `Err` carries the build failure's
    /// text for the caller to notice with.
    pub(crate) fn rebuild_agent(
        &mut self,
        session: Session,
        stats: Option<rhema_transcript::TranscriptStats>,
        sections: Vec<rhema_transcript::Section>,
    ) -> Result<(), String> {
        let (mut agent, subagents) = self.build_agent(session).map_err(|err| err.to_string())?;
        if let Some(stats) = &stats {
            agent.carry_transcript_stats(stats);
        }
        agent.adopt_transcript_sections(sections);
        self.agent = Some(agent);
        self.subagents = Some(subagents);
        Ok(())
    }

    /// Rebuilds the live agent in place: take it, carry its
    /// transcript, build over its session, reinstall. `stamp` clears
    /// the session's environment stamp first — for callers whose
    /// change alters what the machine reports. No agent, nothing to
    /// rebuild.
    pub(crate) fn rebuild_live_agent(&mut self, stamp: bool) -> Result<(), String> {
        let Some(agent) = self.agent.take() else {
            return Ok(());
        };
        let stats = agent.transcript_stats();
        let sections = agent.transcript_sections();
        let mut session = agent.session().clone();
        if stamp {
            rhema_coding_agent::clear_environment_stamp(&mut session);
        }
        self.rebuild_agent(session, stats, sections)
    }

    /// Builds an agent over `session` with the current model and flags.
    /// The wire's registry handle rides beside the agent: a rebuilt
    /// agent carries a fresh registry, and the UI must read that one.
    pub(crate) fn build_agent(
        &self,
        session: Session,
    ) -> Result<(CodingAgent, rhema_subagents::SubagentHandle), CodingAgentError> {
        let provider = self
            .provider
            .clone()
            .ok_or(CodingAgentError::Agent(AgentError::MissingProvider))?;
        let (agent_config, subagents) = session_agent_config(
            &self.options,
            self.sandboxed.as_ref(),
            self.effective().model.clone(),
            provider.clone(),
            &self.config.skills_disabled.clone(),
            &self.config.disabled_subagents,
            &self.effective(),
            self.session_profile
                .as_deref()
                .unwrap_or(rhema_config::DEFAULT_PROFILE),
            &self.config.disabled_mcp_servers,
        );
        Ok((
            CodingAgent::from_session(provider, agent_config, session)?,
            subagents,
        ))
    }
}
