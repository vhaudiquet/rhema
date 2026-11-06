//! The connection's state: [`App`], one slot per live session, the
//! slot bookkeeping the prompt lifecycle leans on (take, put, cancel,
//! evict), and the config/select and slash-command machinery.

use super::wire::*;
use super::*;

/// The most live sessions one connection holds. Real clients keep a
/// handful open; the cap is the server's guard, not a quota clients
/// will feel.
const MAX_SLOTS: usize = 16;

/// One live session: the agent between turns, the cancel sender of the
/// turn in flight, and the `always` answers the session has collected.
pub(crate) struct SessionSlot {
    /// The provider kind the session rides on — a [`backend::PROVIDERS`]
    /// name, or `"mock"` under the mock feature. A model switch may
    /// change it; the provider moves with it.
    pub(crate) kind: &'static str,
    /// The provider the agent's turns run on; a model switch that names
    /// another kind replaces it.
    pub(crate) provider: Arc<dyn ModelProvider>,
    /// Gone while a turn runs; the prompt task puts it back after.
    pub(crate) agent: Option<CodingAgent>,
    /// The last moment this slot served a request. Eviction reads it:
    /// the oldest idle slot goes when the map passes its cap.
    pub(crate) last_used: std::time::Instant,
    /// The model the agent runs on. A `session/set_config_option` that
    /// names a different model rebuilds the agent over the session's
    /// history; the slot's `model` is the current value the picker shows.
    pub(crate) model: String,
    /// The workspace root the agent's tools are scoped to; a model
    /// change rebuilds the agent over the same history and root.
    pub(crate) workspace: std::path::PathBuf,
    /// The catalog rows the model picker offers — every logged-in
    /// provider's, fetched once per `session/new`/`session/load`. A
    /// model switch re-renders the select from these rows without
    /// re-fetching.
    pub(crate) catalog_rows: Vec<backend::CatalogRow>,
    /// The reasoning effort the session runs on; a `thinking` switch
    /// (config option or command) rebuilds the agent over the same
    /// history and model.
    pub(crate) thinking: ThinkingEffort,
    /// The current model's context window and prices, read from its
    /// catalog card; the usage updates and the turn meter price by
    /// them. `None` when the card does not say.
    pub(crate) context_size: Option<u64>,
    pub(crate) pricing: Option<rhema_provider::Pricing>,
    /// The meter the session's turns record into: cumulative tokens
    /// and cost, alive across turns and model switches.
    pub(crate) meter: Arc<Mutex<crate::sink::SessionMeter>>,
    /// The title the session's first prompt earns, sent once as a
    /// `SessionInfoUpdate` and then never again; `titled` marks the
    /// send, so later prompts never re-open the question.
    pub(crate) title: Option<String>,
    pub(crate) titled: bool,
    /// The watch sender the prompt task listens to; `None` when idle.
    pub(crate) cancel: Option<watch::Sender<bool>>,
    /// Shared with the session's approval hooks.
    pub(crate) memory: Arc<Mutex<AlwaysMemory>>,
}

/// The connection-wide state every handler closes over.
/// Puts a turn's agent back in its slot however the task ends. The
/// outcome arms take the agent and restore it themselves; a panic
/// unwinding past them is what the drop covers — without it, a slot
/// whose agent never came home serves no further prompt.
pub(crate) struct SlotRestore {
    pub(crate) app: Arc<App>,
    pub(crate) id: Arc<str>,
    pub(crate) agent: Option<CodingAgent>,
}

impl Drop for SlotRestore {
    fn drop(&mut self) {
        if let Some(mut agent) = self.agent.take() {
            repair_dangling_calls(agent.session_mut());
            self.app.put_agent(&self.id, agent);
        }
    }
}

pub(crate) struct App {
    options: AcpOptions,
    /// The profile resolved once at startup: credentials, model, thinking.
    effective: rhema_config::Profile,
    /// The keychain the profile's `secret_ref`s resolve through.
    secrets: rhema_config::secrets::KeyringSecretStore,
    /// The rendered base prompt from the user's SYSTEM.md, when present.
    base_system: Option<String>,
    /// Where sessions persist; `None` when no cache dir exists (turns
    /// still run; only `session/load` across restarts is lost). The
    /// load handler reads it directly.
    pub(crate) base: Option<std::path::PathBuf>,
    sessions: Mutex<HashMap<String, SessionSlot>>,
    next_id: AtomicU64,
}

impl App {
    /// The config in force: the test override when one is injected,
    /// the stored file otherwise.
    #[cfg(feature = "mock")]
    pub(crate) fn load_config(options: &AcpOptions) -> Result<Settings, rhema_config::ConfigError> {
        if let Some(config) = &options.config {
            return Ok(config.clone());
        }
        Settings::load()
    }

    #[cfg(not(feature = "mock"))]
    pub(crate) fn load_config(
        _options: &AcpOptions,
    ) -> Result<Settings, rhema_config::ConfigError> {
        Settings::load()
    }

    pub(crate) fn new(options: AcpOptions) -> Self {
        let config = match Self::load_config(&options) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("rhema-acp: warning: ignoring unreadable rhema config: {err}");
                Settings::default()
            }
        };
        let effective = config.effective(options.profile.as_deref());
        let base_system = rhema_config::Home::discover()
            .ok()
            .map(|home| home.system_prompt())
            .and_then(|path| std::fs::read_to_string(path).ok());
        let base = options
            .session_dir
            .clone()
            .or_else(|| store::base_dir().ok());
        Self {
            options,
            effective,
            secrets: rhema_config::secrets::KeyringSecretStore::named(
                rhema_config::secrets::RHEMA_SERVICE,
            ),
            base_system,
            base,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// The backend the startup options and stored config resolve to;
    /// `None` is `auth_required` to the client. Under the `mock`
    /// feature, a scripted provider replaces resolution entirely.
    pub(crate) fn backend(&self) -> Option<backend::Resolved> {
        #[cfg(feature = "mock")]
        if let Some(replies) = &self.options.mock_replies {
            return Some(backend::Resolved {
                kind: "mock",
                provider: Arc::new(rhema_core::MockProvider::new(replies.iter().cloned())),
                model: self
                    .options
                    .model
                    .clone()
                    .unwrap_or_else(|| "mock".to_owned()),
            });
        }
        // A configuration problem (nothing configured, an unknown
        // stored spelling) is reported on stderr — the one channel left
        // — and reads to the client as `None`: `auth_required`, the
        // honest answer for a session that cannot start either way.
        match backend::resolve(
            &self.effective,
            self.options.provider.as_deref(),
            self.options.model.as_deref(),
            self.options.llama_url.as_deref(),
            Some(&self.secrets),
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                eprintln!("rhema-acp: {err}");
                None
            }
        }
    }

    /// The catalog rows the session's model picker offers: every
    /// provider the profile holds credentials for (the same set the
    /// TUI's `/model` picker offers). Under the `mock` feature there is
    /// no real provider: the scripted model alone is the row, so the
    /// picker still names it.
    pub(crate) async fn catalog(&self) -> Vec<backend::CatalogRow> {
        #[cfg(feature = "mock")]
        {
            // The mock row only stands when a scripted provider is
            // actually configured; a real backend that resolved beside
            // it is not ours to fake a catalog for.
            match self.options.mock_replies.as_ref() {
                Some(_) => {
                    let model = self
                        .options
                        .model
                        .clone()
                        .unwrap_or_else(|| "mock".to_owned());
                    vec![backend::CatalogRow {
                        provider: "mock",
                        card: rhema_provider::ModelCard {
                            id: model.clone(),
                            display_name: model,
                            description: String::new(),
                            // A fake-but-plausible window and rate
                            // card, so the usage updates and the cost
                            // math have something to ride on in tests.
                            input_token_limit: Some(200_000),
                            output_token_limit: None,
                            supports_vision: None,
                            generation_methods: vec!["generateContent".to_owned()],
                            pricing: Some(rhema_provider::Pricing {
                                input_per_mtok: 1.0,
                                output_per_mtok: 3.0,
                                cached_input_per_mtok: None,
                            }),
                        },
                    }]
                }
                None => Vec::new(),
            }
        }
        #[cfg(not(feature = "mock"))]
        backend::catalog(
            &self.effective,
            self.options.llama_url.as_deref(),
            Some(&self.secrets),
        )
        .await
    }

    pub(crate) fn thinking(&self) -> ThinkingEffort {
        self.options
            .thinking
            .unwrap_or_else(|| self.effective.thinking_effort())
    }

    pub(crate) fn agent_config(
        &self,
        thinking: ThinkingEffort,
        model: &str,
        workspace: &std::path::Path,
    ) -> CodingAgentConfig {
        let mut config = CodingAgentConfig {
            max_turn_steps: self
                .options
                .max_steps
                .unwrap_or(CodingAgentConfig::DEFAULT_MAX_TURN_STEPS),
            thinking,
            base_system: self.base_system.clone(),
            ..CodingAgentConfig::new(model, workspace)
        };
        // The `bash` tool does its own I/O here (no fs/terminal
        // callbacks), so commands run on this host and the vision
        // wiring works; the map carries it to the shell and the prompt
        // section documents the function.
        if let Some(wiring) = rhema_agent_tools::vision::wire(std::env::var_os("BASH_ENV")) {
            config.shell_env.extend(wiring.shell_env.clone());
            config.vision = Some(wiring.section.to_owned());
        }
        config
    }

    /// The card of the model a session runs on, from its catalog rows.
    pub(crate) fn card_for<'a>(
        rows: &'a [backend::CatalogRow],
        kind: &str,
        model: &str,
    ) -> Option<&'a rhema_provider::ModelCard> {
        rows.iter()
            .find(|row| row.provider == kind && row.card.id == model)
            .map(|row| &row.card)
    }

    /// The current model's window and prices, from the session's rows.
    pub(crate) fn card_facts(
        rows: &[backend::CatalogRow],
        kind: &str,
        model: &str,
    ) -> (Option<u64>, Option<rhema_provider::Pricing>) {
        match Self::card_for(rows, kind, model) {
            Some(card) => (card.input_token_limit, card.pricing),
            None => (None, None),
        }
    }

    /// Both selects a session carries, current values first rendered:
    /// the model it runs on and the effort it thinks with.
    pub(crate) fn config_selects(
        &self,
        kind: &str,
        model: &str,
        thinking: ThinkingEffort,
        rows: &[backend::CatalogRow],
    ) -> Vec<SessionConfigOption> {
        let mut selects = self.model_select(kind, model, rows);
        selects.push(self.thinking_select(thinking));
        selects
    }

    /// The `Thinking` select: the session's current effort and the
    /// levels a client can pick, each naming its tradeoff. `Default`
    /// is not exposed: a session stored at default reads as `Medium`,
    /// the level an unset one resolves to. Display names are
    /// capitalized; the wire ids stay lowercase.
    pub(crate) fn thinking_select(&self, current: ThinkingEffort) -> SessionConfigOption {
        let choices = [
            (
                ThinkingEffort::Low,
                "Low thinking effort, faster answers, smaller problems.",
            ),
            (ThinkingEffort::Medium, "Default. Adapted for most."),
            (
                ThinkingEffort::High,
                "High thinking effort, slower answers, bigger problems.",
            ),
        ];
        let options: Vec<SessionConfigSelectOption> = choices
            .iter()
            .map(|(effort, description)| {
                SessionConfigSelectOption::new(effort_id(*effort), effort_name(*effort))
                    .description(*description)
            })
            .collect();
        SessionConfigOption::select("thinking", "Thinking", effort_id(exposed(current)), options)
            .category(Some(SessionConfigOptionCategory::ThoughtLevel))
    }

    /// The `Model` select one session carries: the model it runs on is
    /// the current value — `<kind>/<model>` — and every catalog row is
    /// a choice, led by the current model when the catalog does not
    /// list it (a `--model` override, or its provider's slice failed to
    /// fetch). Each choice's description carries its provider and the
    /// card's facts — the same line the TUI's picker renders — so the
    /// client can show pricing and context window beside the name.
    pub(crate) fn model_select(
        &self,
        kind: &str,
        model: &str,
        rows: &[backend::CatalogRow],
    ) -> Vec<SessionConfigOption> {
        let current = format!("{kind}/{model}");
        let mut options = Vec::with_capacity(rows.len() + 1);
        let current_card = rows
            .iter()
            .find(|row| row.provider == kind && row.card.id == model)
            .map(|row| &row.card);
        options.push(
            SessionConfigSelectOption::new(current.clone(), model.to_owned()).description(
                match current_card {
                    Some(card) => card_description(kind, card),
                    None => kind.to_owned(),
                },
            ),
        );
        for row in rows {
            let value = format!("{}/{}", row.provider, row.card.id);
            if value == current {
                continue;
            }
            options.push(
                SessionConfigSelectOption::new(value, option_name(&row.card))
                    .description(card_description(row.provider, &row.card)),
            );
        }
        vec![
            SessionConfigOption::select("model", "Model", current, options)
                .category(Some(SessionConfigOptionCategory::Model)),
        ]
    }

    /// A fresh, unique, file-safe session id.
    pub(crate) fn fresh_id(&self) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("rhema-{millis:016x}-{n:03}")
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionSlot>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the agent out for a turn; `None` when the session is
    /// unknown or already running one.
    pub(crate) fn take_agent(&self, id: &str) -> Option<(CodingAgent, Arc<Mutex<AlwaysMemory>>)> {
        let mut sessions = self.lock();
        let slot = sessions.get_mut(id)?;
        let agent = slot.agent.take()?;
        slot.last_used = std::time::Instant::now();
        Some((agent, slot.memory.clone()))
    }

    /// Drops idle slots past [`MAX_SLOTS`] — least recently used
    /// first, never one whose turn is in flight (its agent is checked
    /// out, its cancel armed). A session dropped here is not lost: any
    /// session that ran a turn was persisted at the turn's end, and
    /// `session/load` rebuilds the slot from disk. The map is a
    /// server's whole working set; without this, one long-lived
    /// connection holds every session's agent forever.
    pub(crate) fn evict_idle(&self) {
        let mut sessions = self.lock();
        while sessions.len() > MAX_SLOTS {
            let oldest = sessions
                .iter()
                .filter(|(_, slot)| slot.agent.is_some() && slot.cancel.is_none())
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                // Every slot is mid-turn; try again at the next install.
                return;
            };
            sessions.remove(&oldest);
        }
    }

    /// Installs a cancel sender for the turn about to run.
    pub(crate) fn set_cancel(&self, id: &str, cancel: Option<watch::Sender<bool>>) {
        if let Some(slot) = self.lock().get_mut(id) {
            slot.cancel = cancel;
        }
    }

    /// Fires the cancel flag of the session's running turn. No turn, no
    /// effect: cancellation is idempotent by protocol.
    pub(crate) fn cancel(&self, id: &str) {
        if let Some(slot) = self.lock().get(id)
            && let Some(cancel) = &slot.cancel
        {
            let _ = cancel.send(true);
        }
    }

    /// Puts the agent back after a turn and clears the cancel sender.
    pub(crate) fn put_agent(&self, id: &str, agent: CodingAgent) {
        if let Some(slot) = self.lock().get_mut(id) {
            slot.agent = Some(agent);
            slot.cancel = None;
        }
    }

    /// Swaps the session's model — and, when the choice names another
    /// provider kind, the provider with it: the agent is rebuilt over
    /// the session's existing history, so the conversation continues
    /// under the new model. Returns both selects with their new
    /// current values (the client re-renders its pickers). An unknown
    /// session or a turn in flight is `invalid_request`; a bad choice
    /// value or a kind whose credentials do not resolve is
    /// `invalid_params` naming the fix.
    pub(crate) fn switch_model(
        &self,
        id: &str,
        value: &str,
    ) -> Result<Vec<SessionConfigOption>, Error> {
        // The swap runs under the lock; the selects are built once it
        // is released, since `model_select` borrows `self` again.
        let (kind, model, thinking, rows) = {
            let mut sessions = self.lock();
            let slot = sessions.get_mut(id).ok_or_else(|| {
                Error::invalid_request()
                    .data("unknown session, or a turn is already running for it")
            })?;
            let (kind, model) = backend::split_switch(value, slot.kind)
                .map_err(|message| Error::invalid_params().data(message))?;
            let provider = match kind {
                Some(kind) => backend::provider_for(
                    kind,
                    &self.effective,
                    self.options.llama_url.as_deref(),
                    Some(&self.secrets),
                )
                .map_err(|message| Error::invalid_params().data(message))?
                .ok_or_else(|| {
                    Error::invalid_params().data(format!(
                        "no usable credentials for {kind}; log in once with the \
                         rhema TUI or set its API key"
                    ))
                })?,
                None => slot.provider.clone(),
            };
            let kind = kind.unwrap_or(slot.kind);
            let session = slot
                .agent
                .as_ref()
                .ok_or_else(|| {
                    Error::invalid_request()
                        .data("unknown session, or a turn is already running for it")
                })?
                .session()
                .clone();
            let agent = CodingAgent::from_session(
                provider.clone(),
                self.agent_config(slot.thinking, &model, &slot.workspace),
                session,
            )
            .map_err(|err| Error::internal_error().data(err.to_string()))?;
            slot.kind = kind;
            slot.provider = provider;
            slot.model = model.clone();
            slot.agent = Some(agent);
            let rows = slot.catalog_rows.clone();
            let (context_size, pricing) = Self::card_facts(&rows, kind, &model);
            slot.context_size = context_size;
            slot.pricing = pricing;
            let thinking = slot.thinking;
            (kind, model, thinking, rows)
        };
        Ok(self.config_selects(kind, &model, thinking, &rows))
    }

    /// Swaps the session's reasoning effort: the agent is rebuilt over
    /// the session's existing history and current model, so only the
    /// thinking changes. Returns both selects, as a model switch does.
    pub(crate) fn switch_thinking(
        &self,
        id: &str,
        value: &str,
    ) -> Result<Vec<SessionConfigOption>, Error> {
        let effort = effort_of(value).ok_or_else(|| {
            Error::invalid_params().data(format!(
                "thinking expects low, medium, or high; got {value:?}"
            ))
        })?;
        // The swap runs under the lock, as a model switch does.
        let (kind, model, rows) = {
            let mut sessions = self.lock();
            let slot = sessions.get_mut(id).ok_or_else(|| {
                Error::invalid_request()
                    .data("unknown session, or a turn is already running for it")
            })?;
            let session = slot
                .agent
                .as_ref()
                .ok_or_else(|| {
                    Error::invalid_request()
                        .data("unknown session, or a turn is already running for it")
                })?
                .session()
                .clone();
            let agent = CodingAgent::from_session(
                slot.provider.clone(),
                self.agent_config(effort, &slot.model, &slot.workspace),
                session,
            )
            .map_err(|err| Error::internal_error().data(err.to_string()))?;
            slot.thinking = effort;
            slot.agent = Some(agent);
            (slot.kind, slot.model.clone(), slot.catalog_rows.clone())
        };
        Ok(self.config_selects(kind, &model, effort, &rows))
    }

    /// The commands a prompt's first line can name. A client lists
    /// them from this update; the same names route in `session/prompt`.
    pub(crate) fn available_commands(&self) -> AvailableCommandsUpdate {
        AvailableCommandsUpdate::new(vec![
            AvailableCommand::new(
                "thinking",
                "Switch the reasoning effort this session thinks with",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("low, medium, or high"),
            )),
            AvailableCommand::new("model", "Switch the model this session runs on").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                    "provider/model, as the model picker lists them",
                )),
            ),
        ])
    }

    /// Runs a prompt whose first line names one of the session's
    /// commands: the effort or model switch happens in place (no model
    /// call, no history entry), the answer streams as the turn's
    /// message, and the selects ride along so pickers re-render.
    /// `None` when the name is not one of ours — the prompt then runs
    /// as a normal turn.
    pub(crate) fn command(
        &self,
        id: &str,
        name: &str,
        args: &str,
    ) -> Result<Option<CommandAnswer>, Error> {
        match name {
            "thinking" => Ok(Some(self.thinking_command(id, args)?)),
            "model" => Ok(Some(self.model_command(id, args)?)),
            _ => Ok(None),
        }
    }

    /// `/thinking`: no argument names the current effort, an argument
    /// switches it; a bad argument answers with the valid ones.
    pub(crate) fn thinking_command(&self, id: &str, args: &str) -> Result<CommandAnswer, Error> {
        if args.is_empty() {
            let (thinking, selects) = {
                let sessions = self.lock();
                let slot = sessions.get(id).ok_or_else(|| {
                    Error::invalid_request()
                        .data("unknown session, or a turn is already running for it")
                })?;
                let selects =
                    self.config_selects(slot.kind, &slot.model, slot.thinking, &slot.catalog_rows);
                (slot.thinking, selects)
            };
            return Ok(CommandAnswer {
                message: format!(
                    "reasoning effort is {} — pass one of low, medium, high to switch",
                    effort_id(exposed(thinking))
                ),
                selects,
            });
        }
        match self.switch_thinking(id, args) {
            Ok(selects) => Ok(CommandAnswer {
                message: format!(
                    "reasoning effort set to {}",
                    effort_id(effort_of(args).expect("switch_thinking parsed it"))
                ),
                selects,
            }),
            // A switch that cannot run (a turn in flight, an unknown
            // session) answers as an error; a bad argument answers in
            // band with the valid ones.
            Err(err) => Ok(CommandAnswer {
                message: format!("{err}"),
                selects: Vec::new(),
            }),
        }
    }

    /// `/model`: no argument names the current model and every choice
    /// the picker offers, an argument switches to it; a bad argument
    /// answers with the failure the switch names.
    pub(crate) fn model_command(&self, id: &str, args: &str) -> Result<CommandAnswer, Error> {
        if args.is_empty() {
            let (kind, model, rows, selects) = {
                let sessions = self.lock();
                let slot = sessions.get(id).ok_or_else(|| {
                    Error::invalid_request()
                        .data("unknown session, or a turn is already running for it")
                })?;
                let selects =
                    self.config_selects(slot.kind, &slot.model, slot.thinking, &slot.catalog_rows);
                (
                    slot.kind,
                    slot.model.clone(),
                    slot.catalog_rows.clone(),
                    selects,
                )
            };
            let mut choices: Vec<String> = rows
                .iter()
                .map(|row| format!("{}/{}", row.provider, row.card.id))
                .collect();
            let current = format!("{kind}/{model}");
            choices.retain(|choice| *choice != current);
            choices.insert(0, current);
            return Ok(CommandAnswer {
                message: format!(
                    "model is {} — pass one of: {}",
                    choices[0],
                    choices.join(", ")
                ),
                selects,
            });
        }
        match self.switch_model(id, args) {
            Ok(selects) => Ok(CommandAnswer {
                message: format!("model switched to {args}"),
                selects,
            }),
            Err(err) => Ok(CommandAnswer {
                message: format!("{err}"),
                selects: Vec::new(),
            }),
        }
    }

    /// Remembers the title the session's first prompt earns: its first
    /// non-empty line, kept to a client-friendly width. Later prompts
    /// leave the title alone.
    pub(crate) fn remember_title(&self, id: &str, prompt: &str) {
        let mut sessions = self.lock();
        if let Some(slot) = sessions.get_mut(id)
            && !slot.titled
        {
            slot.title = title_of(prompt);
        }
    }

    /// Reports the session's activity: the pending title (once) and
    /// the last-activity timestamp, for clients that list sessions.
    pub(crate) fn note_activity(&self, id: &str, cx: &ConnectionTo<Client>) {
        let update = {
            let mut sessions = self.lock();
            let Some(slot) = sessions.get_mut(id) else {
                return;
            };
            let mut info = SessionInfoUpdate::new().updated_at(rfc3339_now());
            if let Some(title) = slot.title.take() {
                info = info.title(title);
                slot.titled = true;
            }
            SessionUpdate::SessionInfoUpdate(info)
        };
        let _ = cx.send_notification(SessionNotification::new(SessionId::new(id), update));
    }

    /// The session's cumulative meter, for the turn about to run.
    pub(crate) fn meter(&self, id: &str) -> Option<Arc<Mutex<crate::sink::SessionMeter>>> {
        self.lock().get(id).map(|slot| slot.meter.clone())
    }

    /// Persists a session after a completed turn. Best effort: a failed
    /// write warns on stderr and the turn still succeeds; stdout stays
    /// reserved for protocol traffic.
    pub(crate) fn persist(&self, id: &str, session: &Session) {
        let Some(base) = &self.base else {
            return;
        };
        if let Err(err) = store::save_session(base, id, session) {
            eprintln!("rhema-acp: warning: session {id} was not saved: {err}");
        }
    }
}

/// The answer a command turn streams: the text and the selects that
/// keep the client's pickers current.
pub(crate) struct CommandAnswer {
    pub(crate) message: String,
    pub(crate) selects: Vec<SessionConfigOption>,
}
