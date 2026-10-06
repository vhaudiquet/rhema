//! The application state machine: screens, transitions, and the routing of
//! keypresses and UI events. Pure logic — effects that touch the network
//! or spawn turns are returned to the runner (`crate::run`), which owns
//! the runtime and the terminal.

use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig, CodingAgentError};
use rhema_config::Config;
use rhema_core::{AgentError, ModelOutput, ModelProvider, Session, ToolCall, Usage};
use rhema_provider_google::GeminiModel;

use crate::approval::ApprovalAnswerer;
use crate::events::{FetchWhy, UiEvent};
use crate::screens::{
    ChatAction, ChatState, LoginAction, LoginScreen, LoginStatus, PickerAction, PickerPurpose,
    PickerScreen, PickerStatus,
};
use crate::store::ConfigStore;
use crate::transcript::Transcript;

/// Everything the runner fixed before the app started: flags, store,
/// resolved credentials, and the initial session.
pub struct AppSetup {
    /// Config persistence seam.
    pub store: Arc<dyn ConfigStore>,
    /// The loaded config (login and model choices persist here).
    pub config: Config,
    /// The resolved backend, `None` when the login screen must run first.
    pub backend: Option<Backend>,
    /// The starting session: empty, or resumed from `--resume`.
    pub session: Session,
    /// The startup flags.
    pub options: RunOptions,
}

/// The model backend a session runs on: which provider, and whatever
/// credentials or endpoints it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Google Gemini, by API key.
    Google {
        /// The resolved key (`env` first, stored second).
        api_key: String,
    },
    /// A llama.cpp server (`llama-server`, possibly behind a router) by
    /// URL, with an optional bearer key.
    LlamaCpp {
        /// Server base URL.
        url: String,
        /// Bearer key, when the server asks for one.
        api_key: Option<String>,
    },
}

impl Backend {
    /// The provider implementation for this backend.
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        match self {
            Backend::Google { api_key } => {
                Arc::new(rhema_provider_google::GoogleProvider::new(api_key.clone()))
            }
            Backend::LlamaCpp { url, api_key } => {
                let mut provider = rhema_provider_llama_cpp::LlamaCppProvider::new(url.clone());
                if let Some(key) = api_key {
                    provider = provider.with_api_key(key.clone());
                }
                Arc::new(provider)
            }
        }
    }

    /// The config `provider` string this backend persists.
    pub fn config_name(&self) -> &'static str {
        match self {
            Backend::Google { .. } => "google",
            Backend::LlamaCpp { .. } => "llama-cpp",
        }
    }

    /// The models the backend offers, normalized to the picker's row type.
    pub async fn list_models(&self) -> Result<Vec<GeminiModel>, String> {
        match self {
            Backend::Google { api_key } => {
                rhema_provider_google::GoogleProvider::new(api_key.clone())
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            Backend::LlamaCpp { url, api_key } => {
                let mut provider = rhema_provider_llama_cpp::LlamaCppProvider::new(url.clone());
                if let Some(key) = api_key {
                    provider = provider.with_api_key(key.clone());
                }
                let ids = provider
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())?;
                // llama.cpp reports ids only; the picker row degrades to
                // the bare model name.
                Ok(ids
                    .into_iter()
                    .map(|id| GeminiModel {
                        display_name: id.clone(),
                        id,
                        description: String::new(),
                        input_token_limit: None,
                        output_token_limit: None,
                        generation_methods: vec!["generateContent".to_owned()],
                    })
                    .collect())
            }
        }
    }
}

/// The startup flags that outlive the screens.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Which Jadeite theme paints the screen.
    pub theme: crate::args::ThemeKind,
    /// Workspace root for tools.
    pub workspace: PathBuf,
    /// Max steps per turn.
    pub max_steps: u32,
    /// Approve all gated tool calls without asking.
    pub no_confirm: bool,
    /// Checkpoint the session after every turn.
    pub save: Option<PathBuf>,
}

/// The current screen.
pub enum Screen {
    /// No resolvable API key: log in first.
    Login(LoginScreen),
    /// Choose a model.
    Picker(PickerScreen),
    /// The conversation.
    Chat,
}

/// A side effect the runner must perform (network or task spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Fetch the model list for `why`.
    FetchModels(FetchWhy),
    /// Start one turn with this prompt.
    StartTurn(String),
}

/// The whole application state.
pub struct App {
    /// Config persistence seam.
    pub store: Arc<dyn ConfigStore>,
    /// The config as currently held; mutations persist through `store`.
    pub config: Config,
    /// The backend resolved at login (or startup), if any.
    pub backend: Option<Backend>,
    /// The provider behind the backend.
    pub provider: Option<Arc<dyn ModelProvider>>,
    /// The startup flags.
    pub options: RunOptions,
    /// The starting session, used when the first agent is built.
    pub session: Session,
    /// The current screen.
    pub screen: Screen,
    /// Cached model list, shared with the picker when it opens.
    pub models: Vec<GeminiModel>,
    /// The agent; `None` while a turn runs (the turn task owns it).
    pub agent: Option<CodingAgent>,
    /// Whether a turn is in flight.
    pub running: bool,
    /// When the current turn started, for the elapsed-time indicator.
    pub turn_started: Option<std::time::Instant>,
    /// The chat transcript.
    pub transcript: Transcript,
    /// Token accounting of the last finished turn.
    pub usage: Option<Usage>,
    /// The last turn's error, shown in the status line until the next
    /// submit.
    pub error: Option<String>,
    /// The tool call currently awaiting the user's answer.
    pub pending_approval: Option<ToolCall>,
    /// The quit confirmation overlay is up.
    pub confirm_quit: bool,
    /// Every future approval request is declined without a modal (Esc
    /// during a running turn; the turn must finish in place).
    pub decline_all: bool,
    /// Set by the runner to end the event loop.
    pub should_quit: bool,
    /// The resolved design tokens every render pass reads.
    pub theme: &'static crate::palette::Theme,
    /// The chat input state.
    pub chat: ChatState,
    answers: Option<ApprovalAnswerer>,
}

/// One keypress routed to the active screen.
enum Routed {
    Login(LoginAction),
    Picker(PickerAction),
    Chat(ChatAction),
}

impl App {
    /// Builds the app and its initial screen. Errors only when a stored
    /// model should open the chat but the agent cannot be built.
    pub fn new(setup: AppSetup) -> Result<Self, CodingAgentError> {
        let session = setup.session;
        let transcript = Transcript::from_history(&session.history);
        let mut agent = None;
        let screen = match (&setup.backend, &setup.config.model) {
            (Some(_), Some(_)) => {
                let provider = setup
                    .backend
                    .as_ref()
                    .map(Backend::provider)
                    .ok_or(AgentError::MissingProvider)?;
                let agent_config = agent_config(&setup.options, setup.config.model.clone());
                agent = Some(CodingAgent::from_session(
                    provider,
                    agent_config,
                    session.clone(),
                )?);
                Screen::Chat
            }
            (Some(_), None) => Screen::Picker(PickerScreen {
                status: PickerStatus::Loading,
                ..PickerScreen::new(Vec::new(), PickerPurpose::Initial)
            }),
            (None, _) => Screen::Login(LoginScreen::new()),
        };
        Ok(Self {
            // Read before `options` moves into the struct.
            theme: match setup.options.theme {
                crate::args::ThemeKind::Dark => &crate::palette::DARK,
                crate::args::ThemeKind::Light => &crate::palette::LIGHT,
            },
            store: setup.store,
            config: setup.config,
            provider: setup.backend.as_ref().map(Backend::provider),
            backend: setup.backend,
            options: setup.options,
            session,
            screen,
            models: Vec::new(),
            agent,
            running: false,
            turn_started: None,
            transcript,
            usage: None,
            error: None,
            pending_approval: None,
            confirm_quit: false,
            decline_all: false,
            should_quit: false,
            chat: ChatState::new(),
            answers: None,
        })
    }

    /// Effects the runner must perform once, before the first draw.
    pub fn initial_effects(&self) -> Vec<Effect> {
        if self.backend.is_some() {
            vec![Effect::FetchModels(FetchWhy::Bootstrap)]
        } else {
            Vec::new()
        }
    }

    /// Handles one keypress; returns the effects to perform.
    /// Runs a chat slash command. Only meaningful while idle: the submit
    /// path routes here before any turn could start.
    fn handle_command(&mut self, command: &str) {
        match command {
            "help" => self.transcript.push_notice(
                "commands: /help · /clear (fresh session) · /exit (quit) — keys: \
                 ctrl-p model · pgup/pgdn or wheel history · shift+drag to copy text",
            ),
            "clear" => {
                if let Some(agent) = self.agent.as_mut() {
                    agent.session_mut().history.clear();
                }
                self.transcript = Transcript::new();
                self.usage = None;
                self.error = None;
                self.transcript.push_notice("conversation cleared");
            }
            "exit" | "quit" => self.should_quit = true,
            other => self
                .transcript
                .push_notice(&format!("unknown command /{other} — /help lists commands")),
        }
    }

    /// One mouse-wheel tick: scrolls the transcript while in the chat
    /// screen; the other screens navigate by keyboard.
    pub fn on_mouse_scroll(&mut self, up: bool) {
        if matches!(self.screen, Screen::Chat) {
            self.chat.scroll_by(if up { 3 } else { -3 });
        }
    }

    /// Routes one keypress to the current screen, then the app-level
    /// handling (modals, quit, effects).
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // Ctrl-C quits immediately, from any screen, modal or not.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.should_quit = true;
            return Vec::new();
        }
        let routed = match &mut self.screen {
            Screen::Login(login) => Routed::Login(login.handle_key(key)),
            Screen::Picker(picker) => Routed::Picker(picker.handle_key(key)),
            Screen::Chat => Routed::Chat(self.chat.handle_key(key)),
        };
        match routed {
            Routed::Login(LoginAction::Validate) => self.validate_login(),
            Routed::Picker(PickerAction::Choose(id)) => {
                self.choose_model(&id);
                Vec::new()
            }
            Routed::Picker(PickerAction::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Routed::Picker(PickerAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Picker(PickerAction::Retry) => self.retry_fetch(),
            Routed::Login(LoginAction::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Routed::Picker(PickerAction::None) | Routed::Login(LoginAction::None) => Vec::new(),
            Routed::Chat(chat_action) => self.on_chat_key(key, chat_action),
        }
    }

    /// Chat keys that are the app's, not the input box's.
    fn on_chat_key(&mut self, key: KeyEvent, action: ChatAction) -> Vec<Effect> {
        // The approval modal takes every key while it is up.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.answer_approval(true),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.answer_approval(false),
                _ => {}
            }
            return Vec::new();
        }
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.should_quit = true,
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            // Other Ctrl combos (including Ctrl-P mid-turn) are inert.
            if key.code == KeyCode::Char('p') && !self.running {
                return self.open_picker();
            }
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            if self.running {
                // Decline everything pending or upcoming; the turn must
                // finish in place (the engine has no cancel primitive).
                self.decline_all = true;
            } else {
                self.confirm_quit = true;
            }
            return Vec::new();
        }
        match action {
            ChatAction::Submit(text) => {
                if self.running || text.trim().is_empty() {
                    // The input is kept, so typing during a turn is not
                    // lost.
                    Vec::new()
                } else if let Some(command) = text.strip_prefix('/') {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.handle_command(command.trim());
                    Vec::new()
                } else {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.transcript.push_user(&text);
                    self.running = true;
                    self.turn_started = Some(std::time::Instant::now());
                    self.error = None;
                    self.decline_all = false;
                    vec![Effect::StartTurn(text)]
                }
            }
            ChatAction::None => Vec::new(),
        }
    }

    /// The login screen validated a provider's input: fetch the models;
    /// the fetch is the validation (a wrong key or a dead server both
    /// surface as the fetch's error).
    fn validate_login(&mut self) -> Vec<Effect> {
        let Screen::Login(login) = &self.screen else {
            return Vec::new();
        };
        let backend = match login.selected_provider() {
            Some(crate::screens::LoginProvider::LlamaCpp) => {
                let url = if login.input.trim().is_empty() {
                    self.config.llama_cpp_url()
                } else {
                    login.input.trim().to_owned()
                };
                Backend::LlamaCpp {
                    url,
                    api_key: self.config.llama_cpp_api_key(),
                }
            }
            _ => Backend::Google {
                api_key: login.input.clone(),
            },
        };
        vec![Effect::FetchModels(FetchWhy::Login { backend })]
    }

    /// Retry a failed model fetch, per the picker's purpose.
    fn retry_fetch(&mut self) -> Vec<Effect> {
        match &self.screen {
            Screen::Picker(picker) => match picker.purpose {
                PickerPurpose::Initial => {
                    vec![Effect::FetchModels(FetchWhy::Bootstrap)]
                }
                PickerPurpose::FromChat => vec![Effect::FetchModels(FetchWhy::Refresh)],
            },
            _ => Vec::new(),
        }
    }

    /// Ctrl-P mid-session: open the picker over the cached models,
    /// fetching first when the cache is empty.
    fn open_picker(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut status = PickerStatus::Ready;
        if self.models.is_empty() {
            status = PickerStatus::Loading;
            effects.push(Effect::FetchModels(FetchWhy::Refresh));
        }
        self.screen = Screen::Picker(PickerScreen {
            status,
            ..PickerScreen::new(self.models.clone(), PickerPurpose::FromChat)
        });
        effects
    }

    /// Handles one event from the turn/fetch tasks.
    pub fn on_event(&mut self, event: UiEvent) -> Vec<Effect> {
        match event {
            UiEvent::Engine(owned) => {
                self.transcript.apply(&owned);
                Vec::new()
            }
            UiEvent::ModelsLoaded { why, result } => {
                self.on_models_loaded(why, result);
                Vec::new()
            }
            UiEvent::ApprovalRequested { call } => {
                if self.decline_all {
                    // Esc declined the rest of the turn: answer without a
                    // modal so the blocked turn task keeps going.
                    self.send_answer(false);
                } else {
                    self.pending_approval = Some(call);
                }
                Vec::new()
            }
            UiEvent::TurnFinished { agent, result } => {
                self.end_turn(agent, result);
                Vec::new()
            }
        }
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    fn on_models_loaded(&mut self, why: FetchWhy, result: Result<Vec<GeminiModel>, String>) {
        match why {
            FetchWhy::Login { backend } => {
                let Screen::Login(login) = &mut self.screen else {
                    return;
                };
                match result {
                    Ok(models) => {
                        match &backend {
                            Backend::Google { api_key } => {
                                self.config.google_api_key = Some(api_key.clone());
                            }
                            Backend::LlamaCpp { url, api_key } => {
                                self.config.provider = Some("llama-cpp".to_owned());
                                self.config.llama_cpp_url = Some(url.clone());
                                self.config.llama_cpp_api_key = api_key.clone();
                            }
                        }
                        if let Err(err) = self.store.save(&self.config) {
                            login.status = LoginStatus::Error(format!("cannot save config: {err}"));
                            return;
                        }
                        self.backend = Some(backend.clone());
                        self.provider = Some(backend.provider());
                        self.models = models.clone();
                        // A single-model server (plain llama-server rather
                        // than a router) has nothing to pick: take it and
                        // go straight to chat.
                        if models.len() == 1 {
                            let id = models[0].id.clone();
                            self.choose_model(&id);
                            return;
                        }
                        self.screen =
                            Screen::Picker(PickerScreen::new(models, PickerPurpose::Initial));
                    }
                    Err(message) => login.on_validated(&Err(message)),
                }
            }
            FetchWhy::Bootstrap | FetchWhy::Refresh => {
                self.models = result.clone().unwrap_or_default();
                if let Screen::Picker(picker) = &mut self.screen {
                    match result {
                        Ok(models) => {
                            picker.set_models(models);
                            picker.status = PickerStatus::Ready;
                        }
                        Err(message) => picker.status = PickerStatus::Error(message),
                    }
                }
            }
        }
    }

    /// A model was chosen in the picker: persist it and (re)build the
    /// agent, carrying the current session over. The old agent is only
    /// dropped once the new one built, so a rebuild failure cannot strand
    /// the chat.
    fn choose_model(&mut self, id: &str) {
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        self.config.model = Some(id.to_owned());
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return;
        }
        match self.build_agent(session) {
            Ok(agent) => {
                // The screen is still the picker here; its purpose says
                // whether this was a mid-session rebuild.
                let from_chat = matches!(&self.screen, Screen::Picker(picker) if picker.purpose == PickerPurpose::FromChat);
                if from_chat {
                    self.transcript
                        .push_notice(&format!("model switched to {id}"));
                }
                self.agent = Some(agent);
                self.screen = Screen::Chat;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Builds an agent over `session` with the current model and flags.
    fn build_agent(&self, session: Session) -> Result<CodingAgent, CodingAgentError> {
        let provider = self
            .provider
            .clone()
            .ok_or(CodingAgentError::Agent(AgentError::MissingProvider))?;
        let agent_config = agent_config(&self.options, self.config.model.clone());
        Ok(CodingAgent::from_session(provider, agent_config, session)?)
    }

    /// The runner accepted a turn: wire the approval answer channel.
    pub fn begin_turn(&mut self, answers: Option<ApprovalAnswerer>) {
        self.answers = answers;
    }

    /// Answers the modal, if the approval hook is listening (with
    /// `--no-confirm` there is none).
    fn answer_approval(&mut self, approved: bool) {
        self.send_answer(approved);
        self.pending_approval = None;
    }

    fn send_answer(&self, approved: bool) {
        if let Some(answers) = &self.answers {
            answers.answer(approved);
        }
    }

    /// The turn task finished: take the agent back, record the usage or
    /// the error, and drop any stale modal (the turn cannot finish while
    /// an approval is unanswered, so a leftover modal means the answer
    /// channel is gone).
    fn end_turn(&mut self, agent: Box<CodingAgent>, result: Result<ModelOutput, String>) {
        self.agent = Some(*agent);
        self.running = false;
        self.turn_started = None;
        self.decline_all = false;
        self.pending_approval = None;
        self.answers = None;
        match result {
            Ok(output) => self.usage = Some(output.usage),
            Err(message) => self.error = Some(message),
        }
    }
}

/// The coding-agent config implied by the startup flags and the chosen
/// model.
fn agent_config(options: &RunOptions, model: Option<String>) -> CodingAgentConfig {
    CodingAgentConfig {
        max_turn_steps: options.max_steps,
        ..CodingAgentConfig::new(model.unwrap_or_default(), &options.workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::TuiApproval;
    use crate::events::OwnedEvent;
    use crate::store::MemoryStore;
    use crate::transcript::Line;
    use rhema_coding_agent::{Approval, CodingAgentConfig};
    use rhema_core::{Message, Role, Session, StopReason, ToolCall, Usage};
    use rhema_provider_google::{GeminiModel, GoogleProvider};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn model(id: &str) -> GeminiModel {
        GeminiModel {
            id: id.into(),
            display_name: id.into(),
            description: String::new(),
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            generation_methods: vec!["generateContent".into()],
        }
    }

    fn models() -> Vec<GeminiModel> {
        vec![model("m-b"), model("m-a")]
    }

    /// A llama.cpp row: an id with none of the metadata Google reports.
    fn llama_model(id: &str) -> GeminiModel {
        GeminiModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec!["generateContent".to_owned()],
        }
    }

    fn test_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            args: serde_json::json!({ "path": "x" }),

            provider_data: serde_json::Value::Null,
        }
    }

    struct Harness {
        app: App,
        store: Arc<MemoryStore>,
    }

    fn harness(api_key: Option<&str>, model: Option<&str>, session: Session) -> Harness {
        let config = Config {
            google_api_key: api_key.map(str::to_owned),
            model: model.map(str::to_owned),
            ..Config::default()
        };
        let store = Arc::new(MemoryStore::new(config.clone()));
        let backend: Option<Backend> = api_key.map(|key| Backend::Google {
            api_key: key.to_owned(),
        });
        let provider: Option<Arc<dyn ModelProvider>> = backend.as_ref().map(Backend::provider);
        let app = App::new(AppSetup {
            store: store.clone(),
            config,
            backend,
            session,
            options: RunOptions {
                theme: crate::args::ThemeKind::Dark,
                workspace: PathBuf::from("."),
                max_steps: 4,
                no_confirm: false,
                save: None,
            },
        })
        .unwrap();
        Harness { app, store }
    }

    #[test]
    fn no_resolvable_key_opens_the_login_screen() {
        let h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Login(_)));
        assert!(h.app.initial_effects().is_empty());
    }

    #[test]
    fn a_key_without_a_stored_model_opens_a_loading_picker() {
        let mut h = harness(Some("k"), None, Session::default());
        assert!(matches!(h.app.screen, Screen::Picker(_)));
        assert_eq!(
            h.app.initial_effects(),
            vec![Effect::FetchModels(FetchWhy::Bootstrap)]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: Ok(models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.status, PickerStatus::Ready);
                assert_eq!(picker.models.len(), 2);
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn a_key_with_a_stored_model_opens_the_chat() {
        let h = harness(Some("k"), Some("m-a"), Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.app.initial_effects(),
            vec![Effect::FetchModels(FetchWhy::Bootstrap)]
        );
    }

    #[test]
    fn login_success_persists_the_key_and_opens_the_picker() {
        let mut h = harness(None, None, Session::default());
        for c in "secret".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into()
                },
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into(),
                },
            },
            result: Ok(models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.models.len(), 2),
            _ => panic!("expected the picker after a successful login"),
        }
        assert_eq!(
            h.store.load().unwrap().google_api_key.as_deref(),
            Some("secret")
        );
        assert!(matches!(
            h.app.backend,
            Some(Backend::Google { ref api_key }) if api_key == "secret"
        ));
    }

    #[test]
    fn llama_login_validates_by_server_url_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // Move to the llama.cpp row, then type the URL (unmasked input).
        h.app.on_key(key(KeyCode::Down));
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(
                    login.selected_provider(),
                    Some(crate::screens::LoginProvider::LlamaCpp)
                );
                assert_eq!(login.input_label(), "server URL: ");
            }
            _ => panic!("expected the login screen"),
        }
        for c in "http://box:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
            },
            result: Ok(vec![llama_model("qwen3-8b"), llama_model("qwen3-32b")]),
        });
        // Router mode: several models land in the picker with no metadata.
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.models.len(), 2);
                let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(ids, ["qwen3-32b", "qwen3-8b"]);
                assert!(picker.models[0].input_token_limit.is_none());
            }
            _ => panic!("expected the picker after a successful llama login"),
        }
        let config = h.store.load().unwrap();
        assert_eq!(config.provider.as_deref(), Some("llama-cpp"));
        assert_eq!(config.llama_cpp_url.as_deref(), Some("http://box:8080"));
    }

    #[test]
    fn a_single_model_server_auto_selects_and_skips_the_picker() {
        let mut h = harness(None, None, Session::default());
        h.app.on_key(key(KeyCode::Down));
        for c in "http://localhost:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://localhost:8080".into(),
                    api_key: None,
                },
            },
            result: Ok(vec![llama_model("qwen3-8b")]),
        });
        // Exactly one model: no picker, straight to chat on that model.
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(h.store.load().unwrap().model.as_deref(), Some("qwen3-8b"));
    }

    #[test]
    fn login_failure_renders_inline_and_persists_nothing() {
        let mut h = harness(None, None, Session::default());
        for c in "bad-key".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "bad-key".into(),
                },
            },
            result: Err("gemini api error (HTTP 403): denied".into()),
        });
        match &h.app.screen {
            Screen::Login(login) => assert_eq!(
                login.status,
                crate::screens::LoginStatus::Error("gemini api error (HTTP 403): denied".into())
            ),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.store.load().unwrap().google_api_key.is_none());
    }

    #[test]
    fn choosing_a_model_persists_it_and_builds_the_agent() {
        let mut h = harness(Some("k"), None, Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: Ok(models()),
        });
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(effects.is_empty());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(h.store.load().unwrap().model.as_deref(), Some("m-a"));
    }

    #[test]
    fn ctrl_p_rebuild_preserves_the_session() {
        let session = Session {
            history: vec![Message::text(Role::User, "earlier work")],
            driver_state: serde_json::Value::Null,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: Ok(models()),
        });
        // Warm cache: no refetch effect.
        assert!(h.app.on_key(ctrl('p')).is_empty());
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.purpose, PickerPurpose::FromChat),
            _ => panic!("expected the picker"),
        }
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));

        assert!(matches!(h.app.screen, Screen::Chat));
        let agent = h.app.agent.as_ref().expect("rebuilt agent");
        assert_eq!(
            agent.session().history,
            vec![Message::text(Role::User, "earlier work")]
        );
        assert_eq!(h.store.load().unwrap().model.as_deref(), Some("m-b"));
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("model switched to m-b".into()))
        );
    }

    #[test]
    fn esc_when_idle_asks_and_y_quits() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: Ok(models()),
        });
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.confirm_quit);
        assert!(!h.app.should_quit);
        h.app.on_key(key(KeyCode::Char('n')));
        assert!(!h.app.confirm_quit);
        h.app.on_key(key(KeyCode::Esc));
        h.app.on_key(key(KeyCode::Char('y')));
        assert!(h.app.should_quit);
    }

    #[test]
    fn esc_when_running_declines_pending_and_upcoming_approvals() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.running = true;
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.decline_all);
        h.app
            .on_event(UiEvent::ApprovalRequested { call: test_call() });
        assert!(h.app.pending_approval.is_none(), "auto-declined, no modal");
        assert!(!h.app.should_quit, "the turn must finish in place");
    }

    #[test]
    fn the_modal_answer_flows_back_to_the_blocked_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (hook, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || hook.approve(&test_call()));
        h.app.begin_turn(Some(answers));

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.pending_approval.is_some());
        h.app.on_key(key(KeyCode::Char('y')));
        assert!(handle.join().unwrap(), "the y key approves the parked call");
    }

    #[test]
    fn turn_finished_records_usage_and_errors() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let make_agent = || {
            Box::new(
                CodingAgent::new(
                    Arc::new(GoogleProvider::new("k")),
                    CodingAgentConfig::new("m-a", "."),
                )
                .unwrap(),
            )
        };
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            cached_input_tokens: 4,
        };
        h.app.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage,
            }),
        });
        assert!(!h.app.running);
        assert!(h.app.error.is_none());
        assert_eq!(h.app.usage, Some(usage));

        h.app.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Err("turn exceeded the maximum number of iterations (4)".into()),
        });
        assert!(!h.app.running);
        assert_eq!(
            h.app.error.as_deref(),
            Some("turn exceeded the maximum number of iterations (4)")
        );
    }

    #[test]
    fn chat_submit_guards() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: Ok(models()),
        });
        // An empty input never submits.
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(!h.app.running);
        // While a turn runs, typing works but Enter is inert.
        h.app.running = true;
        for c in "hi".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(h.app.chat.input, "hi");
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(h.app.running);
        // Idle again: Enter submits, shows the user line, and blocks
        // re-entry.
        h.app.running = false;
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::StartTurn("hi".into())]
        );
        assert!(h.app.running);
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User("hi".into()))
        );
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn engine_events_stream_into_the_transcript() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta("partial".into())));
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta(" answer".into())));
        assert_eq!(
            h.app.transcript.lines(),
            &[Line::Assistant("partial answer".into())]
        );
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let mut h = harness(None, None, Session::default());
        h.app.on_key(ctrl('c'));
        assert!(h.app.should_quit);
    }

    #[test]
    fn slash_help_prints_commands_without_calling_the_model() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_key(key(KeyCode::Char('/')));
        for c in "help".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(h.app.on_key(key(KeyCode::Enter)), Vec::new());
        assert!(!h.app.running, "no turn starts");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("/clear")
        ));
    }

    #[test]
    fn slash_clear_forges_a_fresh_session() {
        let session = Session {
            history: vec![Message::text(Role::User, "earlier")],
            driver_state: serde_json::Value::Null,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        h.app.handle_command("clear");
        assert!(h.app.agent.unwrap().session().history.is_empty());
        assert!(
            h.app.transcript.lines().len() == 1,
            "only the notice remains"
        );
    }

    #[test]
    fn slash_exit_quits_and_unknown_commands_get_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.handle_command("nope");
        assert!(!h.app.should_quit);
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("unknown command /nope")
        ));
        h.app.handle_command("exit");
        assert!(h.app.should_quit);
    }
}
