//! Model choice and login: the pickers, the catalog merge, the
//! credential installs, and the agent rebuild the changes need.

use super::*;

impl App {
    /// Persists the config through the store. `false` means the store
    /// refused and the error strip carries why, callers that must not
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

    /// The add-instance dialog submitted its credential. On the
    /// settings screen this only persists the provider: no model is
    /// fetched or chosen, and `/model` does that when the user asks. The
    /// onboarding wizard still fetches the catalog to fill its own model
    /// step, which is the next page of that flow.
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
            crate::screens::LoginProvider::OpenAi
            | crate::screens::LoginProvider::OpenAiCompatible => Backend::OpenAi {
                api_key: input,
                base_url,
                // Only the custom-endpoint row shows the choice stage;
                // the OpenAI row leaves it unset, so the route rule
                // decides.
                api: pending.api,
            },
            crate::screens::LoginProvider::OpenRouter => Backend::OpenRouter { api_key: input },
            crate::screens::LoginProvider::Anthropic => Backend::Anthropic { api_key: input },
            crate::screens::LoginProvider::DeepSeek => Backend::DeepSeek { api_key: input },
            crate::screens::LoginProvider::Zai => Backend::Zai {
                api_key: input,
                base_url,
            },
            crate::screens::LoginProvider::OpenCode => Backend::opencode(input, base_url),
            crate::screens::LoginProvider::OpenCodeGo => Backend::opencode(
                input,
                base_url.or_else(|| Some(rhema_provider_opencode::GO_BASE_URL.to_owned())),
            ),
            // A stored instance may name a dedicated deployment; a typed
            // login leaves the public route.
            crate::screens::LoginProvider::Mistral => Backend::Mistral {
                api_key: input,
                base_url,
            },
            // The flow path never validates a typed key: its credential
            // arrives from the flow task, already wrapped in the backend.
            crate::screens::LoginProvider::Copilot
            | crate::screens::LoginProvider::ChatGpt
            | crate::screens::LoginProvider::MistralVibe => {
                return Vec::new();
            }
            crate::screens::LoginProvider::Google => Backend::Google { api_key: input },
        };
        if !matches!(self.screen, Screen::Onboarding(_)) {
            let Screen::Settings(settings) = &self.screen else {
                return Vec::new();
            };
            let tab = settings.profile.clone();
            let instance = pending.name;
            if let Err(err) = self.install_login(&tab, &instance, &backend, &[], false) {
                self.fail_login(err);
                return Vec::new();
            }
            self.land_after_add(&instance);
            return Vec::new();
        }
        vec![Effect::FetchModels(FetchWhy::Login {
            backend,
            instance: Some(pending.name),
        })]
    }

    /// A step of the running login flow: render it on the add-instance
    /// dialog, the settings' modal or the wizard's inline copy. On
    /// completion the settings path only stores the credential, like a
    /// typed key; the wizard fetches the catalog for its model step.
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
                dialog.flow_notice = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Waiting(text) => {
                dialog.flow_waiting = Some(text);
                dialog.flow_notice = None;
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
                dialog.flow_notice = None;
                self.login_cancel = None;
                // The flow's instance was named before the flow started.
                let pending = dialog
                    .pending
                    .as_ref()
                    .map(|pending| (pending.provider, pending.name.clone()));
                let instance = pending.as_ref().map(|(_, name)| name.clone());
                let backend = match pending.map(|(provider, _)| provider) {
                    Some(crate::screens::LoginProvider::Copilot) => {
                        Backend::Copilot { api_key: token }
                    }
                    Some(crate::screens::LoginProvider::ChatGpt) => {
                        Backend::ChatGpt { credential: token }
                    }
                    Some(crate::screens::LoginProvider::MistralVibe) => Backend::MistralVibe {
                        api_key: token,
                        base_url: None,
                    },
                    _ => return Vec::new(),
                };
                if !matches!(self.screen, Screen::Onboarding(_)) {
                    let Screen::Settings(settings) = &self.screen else {
                        return Vec::new();
                    };
                    let tab = settings.profile.clone();
                    let Some(instance) = instance else {
                        return Vec::new();
                    };
                    if let Err(err) = self.install_login(&tab, &instance, &backend, &[], false) {
                        self.fail_login(err);
                        return Vec::new();
                    }
                    self.land_after_add(&instance);
                    return Vec::new();
                }
                vec![Effect::FetchModels(FetchWhy::Login { backend, instance })]
            }
        }
    }

    /// Persists a login: the instance lands in `tab`'s profile (its key
    /// in the secret store when the store takes it, the file when it does
    /// not; a re-login's old secret-store entry goes first), and the
    /// catalog's slice for the instance replaces itself. `activate`
    /// makes the session ride this backend; when false, an existing
    /// active provider is left untouched, and only a profile with no
    /// provider at all adopts the new instance so `/model` can reach
    /// it. Returns whether the profile now points at `backend`. The
    /// error, if any, is for the dialog to render inline.
    pub(crate) fn install_login(
        &mut self,
        tab: &str,
        instance: &str,
        backend: &Backend,
        models: &[ModelCard],
        activate: bool,
    ) -> Result<bool, String> {
        let provider = backend.config_name();
        let (api_key, base_url, api) = match backend {
            Backend::LlamaCpp { url, api_key } => (api_key.clone(), Some(url.clone()), None),
            Backend::OpenAi {
                api_key,
                base_url,
                api,
            } => (Some(api_key.clone()), base_url.clone(), *api),
            Backend::Zai {
                api_key, base_url, ..
            }
            | Backend::OpenCode {
                api_key, base_url, ..
            }
            | Backend::Mistral { api_key, base_url }
            | Backend::MistralVibe { api_key, base_url } => {
                (Some(api_key.clone()), base_url.clone(), None)
            }
            Backend::Google { api_key }
            | Backend::OpenRouter { api_key }
            | Backend::Anthropic { api_key }
            | Backend::DeepSeek { api_key }
            | Backend::Copilot { api_key }
            | Backend::ChatGpt {
                credential: api_key,
            } => (Some(api_key.clone()), None, None),
        };
        let secrets = self.secrets.as_deref();
        let mut secret_delete_failure: Option<String> = None;
        // Onboarding always makes the new provider the session's. The
        // add-only path adopts it only when the session has no backend
        // and the profile names no provider, so a second provider never
        // replaces the model the user runs. A provider already serving
        // from the environment is not displaced either.
        let adopt_unset =
            !activate && self.backend.is_none() && self.effective().provider.is_none();
        let mut adopted = activate;
        self.config.set_for_profile(Some(tab), |settings| {
            // A re-login replaces the instance wholesale; its old
            // store entry must not linger, but a store that refuses
            // the delete must not pass silently either, or the secret
            // outlives the login that named it.
            if let Some(old) = settings.instances.get(instance)
                && let Some(id) = old.secret_ref.as_deref()
                && let Some(store) = secrets
                && let Err(err) = store.delete(id)
            {
                secret_delete_failure = Some(err.to_string());
            }
            if activate || adopt_unset {
                settings.provider = Some(provider.to_owned());
                settings.instance = Some(instance.to_owned());
                adopted = true;
            }
            settings.upsert_instance(
                instance,
                rhema_config::ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: None,
                    secret_ref: None,
                    base_url,
                    api: api.filter(|_| provider == "openai"),
                },
            );
            if let Some(key) = api_key.as_deref() {
                settings.place_key(instance, key, secrets);
            }
        });
        if let Some(err) = secret_delete_failure {
            self.error = Some(format!(
                "re-logged in, but the previous secret-store entry could not be deleted: {err}"
            ));
        }
        self.store
            .save(&self.config)
            .map_err(|err| format!("cannot save config: {err}"))?;
        if adopted {
            // A login that takes the session over speaks for the model
            // too: a `/switch` pick from the previous backend goes.
            self.session_model = None;
            self.session_profile = Some(tab.to_owned());
            self.backend = Some(backend.clone());
            self.provider = Some(backend.provider());
        }
        // The other logins' catalogs stay: a fresh login must not blank
        // the picker down to one provider. The instance's own slice
        // replaces itself, so a stale copy cannot linger, a provider
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
        // The slice that just landed may carry the running model's
        // vision verdict: a login that adopts another provider updates
        // the mask now, not at the next rebuild.
        self.sync_vision_verdict();
        Ok(adopted)
    }

    /// Shows a login error inline on whichever host runs the dialog.
    fn fail_login(&mut self, message: String) {
        match &mut self.screen {
            Screen::Settings(settings) => {
                if let Some(dialog) = &mut settings.dialog {
                    dialog.status = crate::screens::DialogStatus::Error(message);
                }
            }
            Screen::Onboarding(wizard) => {
                if let Some(dialog) = &mut wizard.dialog {
                    dialog.status = crate::screens::DialogStatus::Error(message);
                }
            }
            _ => {}
        }
    }

    /// Leaves the add dialog after a provider-only add: no model pick is
    /// forced, the picker's cache is marked stale, and the chat says how
    /// to choose one.
    fn land_after_add(&mut self, instance: &str) {
        if let Screen::Settings(settings) = &mut self.screen {
            settings.dialog = None;
        }
        self.catalog_stale = true;
        self.screen = Screen::Chat;
        self.transcript
            .push_notice(&format!("added {instance}; /model picks the model to run"));
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    pub(crate) fn on_models_loaded(
        &mut self,
        why: FetchWhy,
        result: Vec<crate::events::CatalogFetch>,
        remaining: usize,
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
                        if let Err(err) =
                            self.install_login(&tab, &instance, &backend, &models, true)
                        {
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
                        // came from: the settings flow opens the Models
                        // page (skipping straight to the chat on a
                        // single-model server), the wizard advances to
                        // its own model step (or takes the single model
                        // straight to the colors step).
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
                                            wizard
                                                .go(crate::onboarding::OnboardingStep::Appearance);
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
                                wizard.picker.set_models(rows);
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
                        // The pick persists to the profile the login
                        // just filled, so it lands on the Models page.
                        self.focus_settings_models();
                        // A login installs the backend it validated: that
                        // plan is the session's numbers now.
                        vec![Effect::FetchUsage]
                    }
                    Some(Err(failure)) => {
                        let message = failure.message();
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
                // asked about, a login that raced this one, survives. A
                // total failure is the picker's error, and a partial one
                // is named in its footer, a missing provider must never
                // look like an empty one.
                // Each slice lands on its own event as its provider
                // answers: it replaces that provider's slice of the
                // catalog, and a slice no fetch asked about, a login that
                // raced this one, survives. A dead provider can therefore
                // never blank the live ones, and never delay them either.
                for entry in result {
                    self.models.retain(|existing| {
                        existing.provider != entry.provider
                            || existing.instance.as_deref() != entry.instance.as_deref()
                    });
                    if let Ok(models) = &entry.result {
                        self.models.extend(models.iter().map(|model| {
                            crate::screens::CatalogModel {
                                provider: entry.provider,
                                instance: entry.instance.clone(),
                                model: model.clone(),
                            }
                        }));
                    }
                    // This provider's verdict replaces itself: one that
                    // answered is no longer named among the missing.
                    self.catalog_unavailable
                        .retain(|name| *name != entry.provider);
                    self.catalog_failed.retain(|name| *name != entry.provider);
                    match &entry.result {
                        Ok(_) => {}
                        Err(CatalogFailure::Unavailable) => {
                            self.catalog_unavailable.push(entry.provider);
                        }
                        Err(CatalogFailure::Error(message)) => {
                            self.catalog_failed.push(entry.provider);
                            self.catalog_error = Some(message.clone());
                        }
                    }
                }
                self.models.sort_by(|a, b| {
                    a.provider
                        .cmp(b.provider)
                        .then(a.instance.cmp(&b.instance))
                        .then(a.model.id.cmp(&b.model.id))
                });
                // The catalog that just landed names the running
                // model's vision too: the agent built before the fetch
                // masks or unmasks to match it.
                self.sync_vision_verdict();
                // The failure lists outlive the screen: a bootstrap fetch
                // usually lands on the chat, and the picker opened later
                // still has to say which providers are unavailable.
                // While providers are still out the page keeps loading
                // and draws the rows that landed; the last to report
                // settles the state.
                if remaining == 0 {
                    self.catalog_inflight = false;
                }
                let status = if remaining > 0 {
                    ModelPickerStatus::Loading
                } else if !self.models.is_empty()
                    || (self.catalog_failed.is_empty() && self.catalog_unavailable.is_empty())
                {
                    ModelPickerStatus::Ready
                } else {
                    // Nothing to list and every provider said why: the
                    // widget's own error, not an empty list.
                    let message = self
                        .catalog_error
                        .clone()
                        .unwrap_or_else(|| "no provider answered".to_owned());
                    ModelPickerStatus::Error(message)
                };
                let fill = |picker: &mut crate::screens::ModelPicker| {
                    picker.set_models(self.models.clone());
                    picker.unavailable = self.catalog_unavailable.clone();
                    picker.failed = self.catalog_failed.clone();
                    picker.status = status.clone();
                };
                if let Screen::Picker(picker) = &mut self.screen {
                    fill(picker);
                }
                // The settings' Models page carries the same widget; a
                // fetch that lands while it is open fills it too, keeping
                // the cursor the user gave it and re-marking the model in
                // use once the catalog names what the profile runs.
                if let Screen::Settings(settings) = &mut self.screen {
                    fill(&mut settings.model_picker);
                    settings.mark_current_model();
                }
                Vec::new()
            }
        }
    }

    /// A model was named for `/model <needle>`: resolve the needle and
    /// switch, or say what went missing. `/model` with no needle opens
    /// the settings' Models page instead.
    pub fn choose_model_by_id(&mut self, needle: &str) -> Vec<Effect> {
        match self.resolve_model(needle) {
            Ok((provider, instance, id)) => self.choose_model(provider, instance.as_deref(), &id),
            Err(notice) => {
                self.push_notice(&notice);
                Vec::new()
            }
        }
    }

    /// A model was named for `/switch <needle>`: the pick re-resolves
    /// the backend and rebuilds the agent like a `/model` pick, but it
    /// stays out of the config, so the profile and the next launch keep
    /// the stored model.
    pub fn choose_session_model_by_id(&mut self, needle: &str) -> Vec<Effect> {
        match self.resolve_model(needle) {
            Ok((provider, instance, id)) => {
                self.choose_session_model(provider, instance.as_deref(), &id)
            }
            Err(notice) => {
                self.push_notice(&notice);
                Vec::new()
            }
        }
    }

    /// Resolves a `/model` or `/switch` needle against the cached
    /// catalog's fully-qualified rows, `provider/id`, the shape the
    /// palette completes and the picker displays, exact match first,
    /// then a unique prefix. A bare id matches nothing on purpose:
    /// several providers can serve one id, so the name carries its
    /// provider (`opencode/glm-5.3-flash`, never just
    /// `glm-5.3-flash`). The active instance wins ties: the same model
    /// id may be served by several instances of one provider. `Err` is
    /// the notice to show.
    fn resolve_model(
        &self,
        needle: &str,
    ) -> Result<(&'static str, Option<String>, String), String> {
        let active = self.effective().instance;
        let qualified =
            |entry: &crate::screens::CatalogModel| format!("{}/{}", entry.provider, entry.model.id);
        let mut tag: Vec<(&'static str, Option<String>, String)> = self
            .models
            .iter()
            .filter(|entry| qualified(entry) == needle)
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
            .filter(|entry| qualified(entry).starts_with(needle))
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
            Some(found) => Ok(found),
            None if exact_count > 1 || prefix_count > 1 => Err(format!(
                "{} models match {needle:?}; name one fully or use /model",
                exact_count.max(prefix_count)
            )),
            None => Err(format!(
                "no model matches {needle:?} in the cached catalog; /model lists them"
            )),
        }
    }

    /// A model was chosen in the picker: persist it and (re)build the
    /// agent, carrying the current session over, and land in the chat.
    /// The old agent is only dropped once the new one built, so a
    /// rebuild failure cannot strand the chat.
    pub(crate) fn choose_model(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
    ) -> Vec<Effect> {
        // The guard every sibling switch holds: with the agent out on
        // a running turn, the apply would read the stale pre-turn
        // session snapshot and end_turn would then hand the old
        // conversation's agent back, sitting it on top of the rebuilt
        // one. The pick waits like /profile's.
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; the model pick waits for it to finish");
            return Vec::new();
        }
        let from_chat =
            matches!(&self.screen, Screen::Picker(_)) || matches!(&self.screen, Screen::Chat);
        match self.apply_model_choice(provider, instance, id) {
            Ok(effects) => {
                // An interactive switch, the picker opened from the
                // chat, or a `/model <needle>` typed there, says where
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

    /// A model was chosen for this session alone (`/switch`): the
    /// override lands in memory, then the backend and the agent rebuild
    /// the way a persisted pick does.
    pub(crate) fn choose_session_model(
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
        match self.apply_session_model_choice(provider, instance, id) {
            Ok(effects) => {
                self.transcript.push_notice(&format!(
                    "model switched to {provider}/{id} for this session; the profile keeps its stored model"
                ));
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
        self.apply_choice(provider, instance, id, true)
    }

    /// The `/switch` half of [`Self::apply_model_choice`]: the pick
    /// overrides the session's model in memory and leaves the config's
    /// stored choice alone.
    pub(crate) fn apply_session_model_choice(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
    ) -> Result<Vec<Effect>, String> {
        self.apply_choice(provider, instance, id, false)
    }

    /// One model pick, persisted or session-only: write the choice where
    /// `effective()` reads it back from, re-resolve the backend, and
    /// rebuild the agent over the same conversation.
    fn apply_choice(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
        persist: bool,
    ) -> Result<Vec<Effect>, String> {
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        if persist {
            // The write follows the session's profile, not the startup
            // flag: after a mid-session /profile the pick must land where
            // effective() reads it back from, profiles never inherit each
            // other's model. The provider, and the instance it came from,
            // moves with the model: a pick from another login's catalog
            // switches the backend this session rides on.
            let profile = self.session_profile.as_deref();
            self.config.set_for_profile(profile, |settings| {
                settings.provider = Some(provider.to_owned());
                settings.instance = instance.map(str::to_owned);
                settings.model = Some(id.to_owned());
            });
            if let Err(err) = self.store.save(&self.config) {
                return Err(format!("cannot save config: {err}"));
            }
            // The stored pick is the session's model again.
            self.session_model = None;
        } else {
            self.session_model = Some(SessionModel {
                provider: provider.to_owned(),
                instance: instance.map(str::to_owned),
                model: id.to_owned(),
            });
        }
        let mut backend = match Backend::from_profile(&self.effective(), self.secret_store()) {
            Some(backend) => backend,
            None => {
                // A session-only pick that cannot resolve leaves no
                // override behind: effective() keeps the stored model,
                // which is what the untouched agent still runs.
                if !persist {
                    self.session_model = None;
                }
                return Err(format!(
                    "no usable credentials for {provider}; /login first"
                ));
            }
        };
        // A model pick stays one conversation: Console keeps seeing the
        // same OpenCode session id.
        if let Some(previous) = &self.backend {
            backend.keep_session(previous);
        }
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        let (saved, sections) = self.outgoing_transcript();
        let saved = saved.or(self.transcript_stats);
        // The model may have changed, and the cache is per-model
        // regardless: the report re-states its facts.
        let mut session = session;
        rhema_host_agent::clear_environment_stamp(&mut session);
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
            self.agent.as_ref().and_then(HostAgent::transcript_stats),
            self.agent
                .as_ref()
                .map(HostAgent::transcript_sections)
                .unwrap_or_default(),
        )
    }

    /// Swaps the session's agent for one built over `session` under
    /// the current profile and flags, the rebuild idiom. The
    /// transcript counters and sections given here ride along: same
    /// conversation, new executor. `Err` carries the build failure's
    /// text for the caller to notice with.
    pub(crate) fn rebuild_agent(
        &mut self,
        session: Session,
        stats: Option<rhema_transcript::TranscriptStats>,
        sections: Vec<rhema_transcript::Section>,
    ) -> Result<(), String> {
        let (mut agent, subagents, missing) =
            self.build_agent(session).map_err(|err| err.to_string())?;
        if let Some(stats) = &stats {
            agent.carry_transcript_stats(stats);
        }
        agent.adopt_transcript_sections(sections);
        self.agent = Some(agent);
        self.subagents = Some(subagents);
        self.missing_tools = missing;
        Ok(())
    }

    /// Rebuilds the live agent in place: take it, carry its
    /// transcript, build over its session, reinstall. `stamp` clears
    /// the session's environment stamp first, for callers whose
    /// change alters what the machine reports. No agent, nothing to
    /// rebuild.
    pub(crate) fn rebuild_live_agent(&mut self, stamp: bool) -> Result<(), String> {
        let Some(agent) = self.agent.take() else {
            if self.turn.running {
                // The agent is out with the running turn's task, and
                // the toggle that reached for this rebuild promised
                // one at the turn's end: nothing else runs it. The
                // flag drains when the agent returns; the OR keeps
                // the stronger stamp of two mid-turn asks.
                self.deferred_rebuild = Some(stamp | self.deferred_rebuild.unwrap_or(false));
            }
            return Ok(());
        };
        let stats = agent.transcript_stats();
        let sections = agent.transcript_sections();
        let mut session = agent.session().clone();
        if stamp {
            rhema_host_agent::clear_environment_stamp(&mut session);
        }
        self.rebuild_agent(session, stats, sections)
    }

    /// Runs the rebuild a running turn deferred, if a mid-turn toggle
    /// queued one. Called from every point an agent returns to the
    /// app: a skipped rebuild would stay skipped, the prompt and the
    /// wiring would stay frozen until some unrelated rebuild.
    pub(crate) fn drain_deferred_rebuild(&mut self) {
        if let Some(stamp) = self.deferred_rebuild.take()
            && let Err(error) = self.rebuild_live_agent(stamp)
        {
            self.push_notice(&error);
        }
    }

    /// A catalog landed: when it names the running model as one that
    /// cannot see, and the live agent was built without knowing, the
    /// agent is rebuilt so the image mask follows the card. A card
    /// that turns out to accept images drops a mask the agent still
    /// carries. The unknown verdict changes nothing either way: a
    /// slice that vanished, a re-login clearing one, must not unmask
    /// a model that may still be blind. A rebuild that cannot build
    /// now (a running turn has the agent) is deferred to the turn's
    /// end, the same queue the other mid-turn toggles use.
    pub(crate) fn sync_vision_verdict(&mut self) {
        match self.current_supports_vision() {
            Some(false) if !self.applied_vision => {}
            Some(true) if self.applied_vision => {}
            _ => return,
        }
        if self.agent.is_none() && !self.turn.running {
            // Nothing built to update, and no build in flight to
            // defer: the next agent reads the verdict itself.
            return;
        }
        if let Err(error) = self.rebuild_live_agent(false) {
            self.push_notice(&error);
        }
    }

    /// Builds an agent over `session` with the current model and flags.
    /// The wire's registry handle rides beside the agent: a rebuilt
    /// agent carries a fresh registry, and the UI must read that one.
    /// The third element is the assembly's absence list, the startup
    /// notice's feed, the rebuild re-probes, so the notice re-answers
    /// with it.
    pub(crate) fn build_agent(
        &mut self,
        session: Session,
    ) -> Result<(HostAgent, rhema_subagents::SubagentHandle, Vec<String>), HostAgentError> {
        let provider = self
            .provider
            .clone()
            .ok_or(HostAgentError::Agent(AgentError::MissingProvider))?;
        // The card's verdict drives the agent's image mask: the
        // session keeps its images, the request does not carry them
        // into a model that cannot read them.
        let supports_vision = self.current_supports_vision();
        let (mut agent_config, subagents, missing) = session_agent_config(
            &self.mcp,
            &self.kits,
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
            Some(&self.jobs_tag),
            self.secrets.as_ref(),
            self.jobs.as_ref().map(crate::jobs::JobsHandle::steering),
            &self.jobs_tag,
        );
        agent_config.supports_vision = supports_vision;
        let agent = HostAgent::from_session(agent_config, session)?;
        self.applied_vision = supports_vision == Some(false);
        Ok((agent, subagents, missing))
    }
}
