//! The application state machine: screens, transitions, and the routing of
//! keypresses and UI events. Pure logic — effects that touch the network
//! or spawn turns are returned to the runner (`crate::run`), which owns
//! the runtime and the terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig, CodingAgentError};
use rhema_config::Config;
use rhema_core::{
    AgentError, Content, Message, ModelOutput, ModelProvider, Session, ToolCall, Usage,
};
use rhema_provider::ModelCard;

use crate::approval::ApprovalAnswerer;
use crate::events::{FetchWhy, OwnedEvent, UiEvent};
use crate::screens::{
    ChatAction, ChatState, LoginAction, LoginPurpose, LoginScreen, LoginStatus, PickerAction,
    PickerPurpose, PickerScreen, PickerStatus, ProfileAction, ProfileScreen,
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
    /// OpenRouter, by API key.
    OpenRouter {
        /// The resolved key (`env` first, stored second).
        api_key: String,
    },
    /// Anthropic, by API key.
    Anthropic {
        /// The resolved key (`env` first, stored second).
        api_key: String,
    },
    /// OpenAI, or any OpenAI-compatible server, by API key: an instance
    /// may name a base URL (a local vLLM, a proxy); `None` speaks to
    /// OpenAI itself.
    OpenAi {
        /// The resolved key (`env` first, stored second).
        api_key: String,
        /// The instance's base URL, when it stored one.
        base_url: Option<String>,
    },
    /// DeepSeek, by API key (their OpenAI-compatible endpoint).
    DeepSeek {
        /// The resolved key (`env` first, stored second).
        api_key: String,
    },
    /// GitHub Copilot, driven by the device flow's long-lived GitHub
    /// OAuth token (the adapter exchanges it for short-lived tokens).
    Copilot {
        /// The resolved GitHub token (`env` first, stored second).
        api_key: String,
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
            Backend::OpenRouter { api_key } => Arc::new(
                rhema_provider_openrouter::OpenRouterProvider::new(api_key.clone()),
            ),
            Backend::Anthropic { api_key } => Arc::new(
                rhema_provider_anthropic::AnthropicProvider::new(api_key.clone()),
            ),
            // An instance's base URL points the adapter at any
            // OpenAI-compatible server; the default is OpenAI itself.
            Backend::OpenAi { api_key, base_url } => {
                let mut provider = rhema_provider_openai_compatible::OpenAiCompatibleProvider::new(
                    api_key.clone(),
                );
                if let Some(base_url) = base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                {
                    provider = provider.with_base_url(base_url.to_owned());
                }
                Arc::new(provider)
            }
            Backend::DeepSeek { api_key } => Arc::new(
                rhema_provider_openai_compatible::OpenAiCompatibleProvider::new(api_key.clone())
                    .with_base_url("https://api.deepseek.com/v1"),
            ),
            Backend::Copilot { api_key } => Arc::new(rhema_provider_copilot::CopilotProvider::new(
                api_key.clone(),
            )),
        }
    }

    /// The config `provider` string this backend persists.
    pub fn config_name(&self) -> &'static str {
        match self {
            Backend::Google { .. } => "google",
            Backend::LlamaCpp { .. } => "llama-cpp",
            Backend::OpenRouter { .. } => "openrouter",
            Backend::Anthropic { .. } => "anthropic",
            Backend::OpenAi { .. } => "openai",
            Backend::DeepSeek { .. } => "deepseek",
            Backend::Copilot { .. } => "copilot",
        }
    }

    /// Builds the backend a profile's settings call for; `None` when the
    /// profile names no provider (or a spelling no provider answers to,
    /// or has no usable credentials for the one it names) — the login
    /// flow runs, no default backend substitutes. `RHEMA_PROVIDER` wins
    /// over the stored `provider`, the same env-wins convention the keys
    /// follow. The active instance serves; a profile without instances
    /// falls back to the environment alone.
    pub fn from_profile(profile: &rhema_config::Profile) -> Option<Backend> {
        let active = profile.provider_name()?;
        if let Some((_, instance)) = profile.instance_for_provider(&active) {
            return Self::from_instance(profile, instance);
        }
        Self::named(&active, profile)
    }

    /// Builds one backend per logged-in instance of the profile — the
    /// active one first, then the rest in name order — so the picker
    /// spans every llama.cpp server, OpenAI-compatible endpoint, and
    /// Copilot sub the profile holds. A profile without instances falls
    /// back to the environment alone (llama.cpp only when active: a
    /// URL is a server address, not a login).
    pub fn candidates_from_profile(profile: &rhema_config::Profile) -> Vec<Candidate> {
        let active = profile.provider_name();
        let active_entry = active
            .as_deref()
            .and_then(|kind| profile.instance_for_provider(kind))
            .map(|(name, _)| name.to_owned());
        let mut candidates = Vec::new();
        if let Some(kind) = active.as_deref() {
            if let Some((name, instance)) = profile.instance_for_provider(kind) {
                if let Some(backend) = Self::from_instance(profile, instance) {
                    candidates.push(Candidate {
                        instance: Some(name.to_owned()),
                        backend,
                    });
                }
            } else if let Some(backend) = Self::named(kind, profile) {
                candidates.push(Candidate {
                    instance: None,
                    backend,
                });
            }
        }
        for (name, instance) in profile.instance_entries() {
            if active_entry.as_deref() == Some(name) {
                continue;
            }
            if let Some(backend) = Self::from_instance(profile, instance) {
                candidates.push(Candidate {
                    instance: Some(name.to_owned()),
                    backend,
                });
            }
        }
        for name in [
            "google",
            "openrouter",
            "anthropic",
            "openai",
            "deepseek",
            "copilot",
        ] {
            if Some(name) == active.as_deref() || profile.instance_for_provider(name).is_some() {
                continue;
            }
            if let Some(backend) = Self::named(name, profile) {
                candidates.push(Candidate {
                    instance: None,
                    backend,
                });
            }
        }
        candidates
    }

    /// Builds the backend one stored instance calls for; `None` when its
    /// provider needs a key and neither the environment nor the instance
    /// carries one. A kind no provider answers to builds nothing — an
    /// unknown spelling never reads as google.
    fn from_instance(
        profile: &rhema_config::Profile,
        instance: &rhema_config::ProviderInstance,
    ) -> Option<Backend> {
        match instance.provider.as_str() {
            "llama-cpp" => Some(Backend::LlamaCpp {
                url: profile
                    .instance_url(instance)
                    .unwrap_or_else(|| profile.llama_cpp_url()),
                api_key: profile.instance_api_key(instance),
            }),
            "openrouter" => profile
                .instance_api_key(instance)
                .map(|api_key| Backend::OpenRouter { api_key }),
            "anthropic" => profile
                .instance_api_key(instance)
                .map(|api_key| Backend::Anthropic { api_key }),
            "openai" => profile
                .instance_api_key(instance)
                .map(|api_key| Backend::OpenAi {
                    api_key,
                    base_url: profile.instance_url(instance),
                }),
            "deepseek" => profile
                .instance_api_key(instance)
                .map(|api_key| Backend::DeepSeek { api_key }),
            "copilot" => profile
                .instance_api_key(instance)
                .map(|api_key| Backend::Copilot { api_key }),
            _ => profile
                .instance_api_key(instance)
                .filter(|_| instance.provider == "google")
                .map(|api_key| Backend::Google { api_key }),
        }
    }

    /// The backend for one provider's config name from the environment
    /// alone, `None` when the environment carries no usable credential
    /// for it. Profiles with a stored instance resolve through
    /// [`Backend::from_profile`] instead.
    fn named(provider: &str, profile: &rhema_config::Profile) -> Option<Backend> {
        match provider {
            "google" => profile
                .google_api_key()
                .map(|api_key| Backend::Google { api_key }),
            "llama-cpp" => Some(Backend::LlamaCpp {
                url: profile.llama_cpp_url(),
                api_key: profile.llama_cpp_api_key(),
            }),
            "openrouter" => profile
                .openrouter_api_key()
                .map(|api_key| Backend::OpenRouter { api_key }),
            "anthropic" => profile
                .anthropic_api_key()
                .map(|api_key| Backend::Anthropic { api_key }),
            "openai" => profile.openai_api_key().map(|api_key| Backend::OpenAi {
                api_key,
                base_url: None,
            }),
            "deepseek" => profile
                .deepseek_api_key()
                .map(|api_key| Backend::DeepSeek { api_key }),
            "copilot" => profile
                .copilot_api_key()
                .map(|api_key| Backend::Copilot { api_key }),
            _ => None,
        }
    }

    /// The models the backend offers, normalized to the picker's row type.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, String> {
        match self {
            Backend::Google { api_key } => {
                rhema_provider_google::GoogleProvider::new(api_key.clone())
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            Backend::OpenRouter { api_key } => {
                rhema_provider_openrouter::OpenRouterProvider::new(api_key.clone())
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            // Each adapter's listing already fills what its endpoint
            // reports and defaults the rest: Anthropic rows carry id and
            // display name only, OpenAI-compatible rows add the server's
            // name/description/context_length when present.
            Backend::DeepSeek { api_key } => {
                rhema_provider_openai_compatible::OpenAiCompatibleProvider::new(api_key.clone())
                    .with_base_url("https://api.deepseek.com/v1")
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            Backend::Anthropic { api_key } => {
                rhema_provider_anthropic::AnthropicProvider::new(api_key.clone())
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            Backend::OpenAi { api_key, base_url } => {
                let mut provider = rhema_provider_openai_compatible::OpenAiCompatibleProvider::new(
                    api_key.clone(),
                );
                if let Some(base_url) = base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                {
                    provider = provider.with_base_url(base_url.to_owned());
                }
                provider.list_models().await.map_err(|err| err.to_string())
            }
            Backend::Copilot { api_key } => {
                rhema_provider_copilot::CopilotProvider::new(api_key.clone())
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())
            }
            Backend::LlamaCpp { url, api_key } => {
                let mut provider = rhema_provider_llama_cpp::LlamaCppProvider::new(url.clone());
                if let Some(key) = api_key {
                    provider = provider.with_api_key(key.clone());
                }
                let models = provider
                    .list_models()
                    .await
                    .map_err(|err| err.to_string())?;
                Ok(models
                    .into_iter()
                    .map(|model| ModelCard {
                        display_name: model.id.clone(),
                        // The server's own context report, so picker rows
                        // and the footer carry a real window.
                        id: model.id,
                        description: String::new(),
                        input_token_limit: model.context_length,
                        output_token_limit: None,
                        generation_methods: vec!["generateContent".to_owned()],
                        pricing: None,
                    })
                    .collect())
            }
        }
    }
}

/// One backend a profile can serve, with the instance it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The instance name in the profile's registry; `None` when the
    /// backend was built from the environment alone.
    pub instance: Option<String>,
    /// The backend to fetch the catalog with.
    pub backend: Backend,
}

/// The startup flags that outlive the screens.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Which Jadeite theme paints the screen.
    pub theme: crate::args::ThemeKind,
    /// Which icon flavor the footer draws with.
    pub icons: crate::icons::Flavor,
    /// The config profile this session runs under.
    pub profile: Option<String>,
    /// Workspace root for tools.
    pub workspace: PathBuf,
    /// Max steps per turn.
    pub max_steps: u32,
    /// How much reasoning the model should do.
    pub thinking: rhema_core::request::ThinkingEffort,
    /// The base prompt from the user's SYSTEM.md, when their file
    /// exists; `None` uses the built-in default.
    pub base_system: Option<String>,
    /// Approve all gated tool calls without asking.
    /// Checkpoint the session after every turn.
    pub save: Option<PathBuf>,
}

/// The current screen. The login screen is boxed: it is by far the
/// largest state (profile registry plus stored section) and `Screen`
/// sits in the hot keypath.
pub enum Screen {
    /// No resolvable API key: log in first.
    Login(Box<LoginScreen>),
    /// Choose a model.
    Picker(PickerScreen),
    /// Switch the profile the session runs under.
    Profile(ProfileScreen),
    /// Pick a saved session of this workspace to continue.
    Resume(crate::screens::ResumeScreen),
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
    /// Spawn the interactive login flow the login screen chose (its
    /// stage is already `Flow`); updates arrive as
    /// `UiEvent::LoginUpdate`.
    StartLoginFlow,
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
    /// The home directory, when the environment names one: the footer
    /// trims it (and its `Documents`) off the workspace path it shows.
    pub home: Option<PathBuf>,
    /// The profile this session runs under: the `--profile` flag over the
    /// stored active profile. `/profile` switches it mid-session.
    pub session_profile: Option<String>,
    /// The starting session, used when the first agent is built.
    pub session: Session,
    /// The current screen.
    pub screen: Screen,
    /// Cached model catalog, shared with the picker when it opens: every
    /// logged-in provider's models, each tagged with its provider.
    pub models: Vec<crate::screens::CatalogModel>,
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
    /// Accumulated cost of the session in USD, summed per turn from the
    /// current model's pricing; turns on unpriced models add nothing.
    pub session_cost: f64,
    /// The last turn's error, shown in the status line until the next
    /// submit.
    pub error: Option<String>,
    /// The tool call currently awaiting the user's answer.
    pub pending_approval: Option<ToolCall>,
    /// The quit confirmation overlay is up.
    pub confirm_quit: bool,
    /// Every future approval request is declined without a modal (the
    /// turn is being cancelled; nothing more may ask).
    pub decline_all: bool,
    /// The cancel flag of the running turn; `None` while idle. The runner
    /// creates the pair at submit and the task watches the receiver.
    pub turn_cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// The cancel handle of the running login flow; `None` while idle.
    /// Dropping it ends the flow task — the watch receiver errors out —
    /// exactly what Esc and a completed flow both want.
    pub login_cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// A command is executing while the turn runs; `false` while the
    /// model itself is streaming — thinking.
    pub processing: bool,
    /// The running turn's reasoning trace, tail only: the status strip
    /// previews its last rows, and it clears the moment real output
    /// starts. Never part of the transcript or the session history.
    pub thinking: String,
    /// The file this session was resumed from, if any: the exit save
    /// updates it in place instead of growing a second entry in /resume.
    pub resumed_from: Option<PathBuf>,
    /// Set by the runner to end the event loop.
    pub should_quit: bool,
    /// The resolved design tokens every render pass reads.
    pub theme: &'static crate::palette::Theme,
    /// The resolved footer glyphs every render pass reads.
    pub icons: &'static crate::icons::Icons,
    /// The chat input state.
    pub chat: ChatState,
    answers: Option<ApprovalAnswerer>,
}

/// One keypress routed to the active screen.
enum Routed {
    Login(LoginAction),
    Picker(PickerAction),
    Profile(ProfileAction),
    Resume(crate::screens::ResumeAction),
    Chat(ChatAction),
}

impl App {
    /// Builds the app and its initial screen. Errors only when a stored
    /// model should open the chat but the agent cannot be built.
    pub fn new(setup: AppSetup) -> Result<Self, CodingAgentError> {
        let session = setup.session;
        let transcript = Transcript::from_history(&session.history);
        // The settings that apply to this session's profile are computed
        // on demand (they change when the model switches or a login lands).
        let mut agent = None;
        // Startup reads straight off the config: App::effective exists on
        // the struct, which does not exist yet.
        let effective = setup.config.effective(setup.options.profile.as_deref());
        let screen = match (&setup.backend, &effective.model) {
            (Some(_), Some(_)) => {
                let provider = setup
                    .backend
                    .as_ref()
                    .map(Backend::provider)
                    .ok_or(AgentError::MissingProvider)?;
                let agent_config = agent_config(&setup.options, effective.model.clone());
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
            (None, _) => {
                let profile_name = setup
                    .options
                    .profile
                    .clone()
                    .unwrap_or_else(|| "personal".to_owned());
                Screen::Login(Box::new(
                    LoginScreen::new().with_registry(&setup.config.profiles, &profile_name),
                ))
            }
        };
        Ok(Self {
            // Read before `options` moves into the struct.
            theme: match setup.options.theme {
                crate::args::ThemeKind::Dark => &crate::palette::DARK,
                crate::args::ThemeKind::Light => &crate::palette::LIGHT,
            },
            icons: setup.options.icons.set(),
            store: setup.store,
            config: setup.config,
            provider: setup.backend.as_ref().map(Backend::provider),
            backend: setup.backend,
            session_profile: Some(
                setup
                    .options
                    .profile
                    .clone()
                    .unwrap_or_else(|| "personal".to_owned()),
            ),
            options: setup.options,
            home: std::env::var("HOME").ok().map(PathBuf::from),
            session,
            screen,
            models: Vec::new(),
            agent,
            running: false,
            turn_started: None,
            transcript,
            usage: None,
            session_cost: 0.0,
            error: None,
            pending_approval: None,
            confirm_quit: false,
            decline_all: false,
            turn_cancel: None,
            login_cancel: None,
            processing: false,
            thinking: String::new(),
            resumed_from: None,
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
        ));
        Vec::new()
    }

    /// In-band decline for a slash input no table row matches.
    pub fn unknown_command(&mut self, name: &str) {
        self.transcript
            .push_notice(&format!("unknown command {name}; /help lists commands"));
    }

    /// A notice into the transcript, styled by the transcript itself.
    pub fn push_notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// Opens the login screen bound to the session's profile: its
    /// logged-in instances, ready to activate, forget, or add to. The
    /// session stays where it is: a successful login lands in a picker
    /// that rebuilds onto it.
    pub fn open_login(&mut self) {
        let profile_name = self
            .session_profile
            .clone()
            .or_else(|| self.options.profile.clone())
            .unwrap_or_else(|| "personal".to_owned());
        self.screen = Screen::Login(Box::new(
            crate::screens::LoginScreen::for_chat()
                .with_registry(&self.config.profiles, &profile_name),
        ));
    }

    /// Drops the conversation: a fresh transcript and a fresh session.
    /// The resumed origin drops with it — what is typed after /clear is
    /// a new conversation, not a continuation of the loaded file.
    pub fn clear_session(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            agent.session_mut().history.clear();
        }
        self.transcript = Transcript::new();
        self.usage = None;
        self.session_cost = 0.0;
        self.error = None;
        self.resumed_from = None;
        self.transcript.push_notice("conversation cleared");
    }

    /// The session to save for /resume, and the file it belongs to: the
    /// live session when its history holds anything, writing back to the
    /// origin when this session was resumed. A session of notices alone
    /// (a switch, a decline) has nothing resumable and saves nothing.
    pub fn export_session(&self) -> Option<(Session, Option<PathBuf>)> {
        let mut session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        // The accumulated cost rides with the save: a resumed session's
        // footer continues where this one ended.
        session.cost_usd = self.session_cost;
        if session.history.is_empty() {
            None
        } else {
            Some((session, self.resumed_from.clone()))
        }
    }

    /// Switches the session to a profile: re-resolves the backend from
    /// that profile's settings, rebuilds the agent carrying the session
    /// over, and notices. The choice is per-session, never persisted. A
    /// profile without usable credentials still switches — the session's
    /// identity is what moves — but the old provider goes with the old
    /// profile: turns wait for /login rather than leaking the previous
    /// profile's key.
    pub fn switch_profile(&mut self, name: &str) {
        if self.running {
            self.transcript
                .push_notice("a turn is running; /profile waits for it to finish");
            return;
        }
        if self.session_profile.as_deref() == Some(name) {
            self.transcript
                .push_notice(&format!("already on profile {name}"));
            return;
        }
        let effective = self.config.effective(Some(name));
        self.options.thinking = effective.thinking_effort().unwrap_or_default();
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let Some(backend) = Backend::from_profile(&effective) else {
            // Never recorded: startup re-validates usability, so the
            // memory naming a keyless profile would only fall through.
            // The agent drops with the provider: a turn here would run
            // on the profile just left.
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
            return;
        };
        let agent_config = agent_config(&self.options, effective.model.clone());
        match CodingAgent::from_session(backend.provider(), agent_config, session) {
            Ok(agent) => {
                self.session_profile = Some(name.to_owned());
                self.backend = Some(backend.clone());
                self.provider = Some(backend.provider());
                self.models.clear();
                self.agent = Some(agent);
                self.screen = Screen::Chat;
                // An explicit switch is a choice: recorded for this
                // workspace now, not at the next turn.
                self.remember_profile();
                if effective.model.is_none() {
                    self.transcript.push_notice(&format!(
                        "switched to profile {name}; /model to pick a model"
                    ));
                } else {
                    self.transcript
                        .push_notice(&format!("switched to profile {name}"));
                }
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Sets the session's thinking effort: persisted into the session's
    /// profile like a model pick, and the agent rebuilds carrying the
    /// conversation so the very next turn asks at the new level.
    pub fn set_thinking(&mut self, effort: rhema_core::request::ThinkingEffort) {
        if self.running {
            self.transcript
                .push_notice("a turn is running; /thinking waits for it to finish");
            return;
        }
        self.options.thinking = effort;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |settings| {
                settings.thinking = (effort != rhema_core::request::ThinkingEffort::Default)
                    .then(|| effort.as_str().to_owned());
            });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return;
        }
        if let Some(agent) = self.agent.take() {
            let session = agent.session().clone();
            match self.build_agent(session) {
                Ok(rebuilt) => self.agent = Some(rebuilt),
                Err(err) => {
                    self.error = Some(err.to_string());
                    return;
                }
            }
        }
        self.transcript
            .push_notice(&format!("thinking effort set to {}", effort.as_str()));
    }

    /// Lists the discovered skills in the transcript: what the model's
    /// system prompt carries, for the human who wrote them.
    pub fn show_skills(&mut self, global: Option<&Path>, workspace: &Path) {
        let skills = rhema_coding_agent::skills::discover(global, workspace);
        if skills.is_empty() {
            self.transcript.push_notice(
                "no skills. a skill is <dir>/SKILL.md with a `description` \
                 frontmatter, under the config directory or .rhema/skills",
            );
            return;
        }
        let mut table = String::from("name | trigger\n--- | ---\n");
        for skill in &skills {
            // A raw pipe would forge table columns.
            let trigger = skill.description.replace('|', "\\|");
            table.push_str(&format!("{} | {}\n", skill.name, trigger));
        }
        self.transcript.push_markdown(table);
    }

    /// The system prompt in the transcript, fenced so it reads verbatim.
    pub fn show_system_prompt(&mut self, prompt: &str) {
        self.transcript
            .push_markdown(format!("```markdown\n{prompt}```"));
    }

    /// Opens the resume picker over the sessions saved for this
    /// workspace. An empty history of saves is a notice, not a screen.
    pub fn open_resume(&mut self) -> Vec<Effect> {
        match crate::sessions::base_dir() {
            Ok(base) => self.open_resume_from(&base),
            Err(err) => {
                self.push_notice(&format!("cannot locate the session cache: {err}"));
                Vec::new()
            }
        }
    }

    /// [`App::open_resume`] against an explicit base, so tests drive real
    /// directories instead of the environment.
    pub fn open_resume_from(&mut self, base: &Path) -> Vec<Effect> {
        if self.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return Vec::new();
        }
        let workspace = std::fs::canonicalize(&self.options.workspace)
            .unwrap_or_else(|_| self.options.workspace.clone());
        match crate::sessions::list_sessions(base, &workspace) {
            Ok(entries) if entries.is_empty() => {
                self.transcript
                    .push_notice("no saved sessions for this directory");
            }
            Ok(entries) => {
                self.screen = Screen::Resume(crate::screens::ResumeScreen::new(
                    entries,
                    crate::sessions::now_millis(),
                ));
            }
            Err(err) => self.error = Some(format!("cannot list saved sessions: {err}")),
        }
        Vec::new()
    }

    /// Loads a saved session: rebuilds the agent on it, re-derives the
    /// transcript from its history, and lands in the chat. The current
    /// conversation is replaced — it keeps its own saved file from its
    /// own exit.
    pub fn resume_session(&mut self, path: &Path) {
        if self.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return;
        }
        let session = match CodingAgent::load_session_from(&path.to_path_buf()) {
            Ok(session) => session,
            Err(err) => {
                self.error = Some(format!("cannot load session: {err}"));
                return;
            }
        };
        let agent = match self.build_agent(session.clone()) {
            Ok(agent) => agent,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        let messages = session.history.len();
        self.session = session;
        self.agent = Some(agent);
        self.resumed_from = Some(path.to_path_buf());
        self.transcript = Transcript::from_history(&self.session.history);
        self.usage = None;
        // The saved session carries what it spent; the footer continues
        // the story instead of quoting a fresh zero.
        self.session_cost = self.session.cost_usd;
        self.chat.scroll = 0;
        self.screen = Screen::Chat;
        self.transcript
            .push_notice(&format!("resumed {messages} messages"));
    }

    /// A bracketed paste landed: into the input, newlines included —
    /// pasting multi-line text must not fire submits. The login's
    /// credential line takes its own, single-line shape.
    pub fn on_paste(&mut self, text: &str) {
        match &mut self.screen {
            Screen::Chat => self.chat.paste(text),
            Screen::Login(login) => login.paste(text),
            _ => {}
        }
    }

    /// Scroll the history with the wheel: the same step as PageUp/PageDown
    /// (one wheel tick), so both controls move in lockstep.
    pub fn on_mouse_scroll(&mut self, up: bool) {
        if matches!(self.screen, Screen::Chat) {
            let step = crate::screens::ChatState::SCROLL_STEP as isize;
            self.chat.scroll_by(if up { step } else { -step });
        }
    }

    /// Routes one keypress to the current screen, then the app-level
    /// handling (modals, quit, effects).
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // Ctrl-C and Ctrl-D (EOF) quit immediately, from any screen,
        // modal or not.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('d') | KeyCode::Char('D')
            )
        {
            self.should_quit = true;
            return Vec::new();
        }
        let routed = match &mut self.screen {
            Screen::Login(login) => Routed::Login(login.handle_key(key)),
            Screen::Picker(picker) => Routed::Picker(picker.handle_key(key)),
            Screen::Profile(profile) => Routed::Profile(profile.handle_key(key)),
            Screen::Resume(resume) => Routed::Resume(resume.handle_key(key)),
            Screen::Chat => {
                let profiles = self.palette_profiles();
                Routed::Chat(self.chat.handle_key(key, &self.models, &profiles))
            }
        };
        match routed {
            Routed::Login(LoginAction::Validate) => self.validate_login(),
            Routed::Login(LoginAction::Activate) => self.activate_instance(),
            Routed::Login(LoginAction::Remove) => self.remove_instance(),
            Routed::Login(LoginAction::StartFlow) => {
                vec![Effect::StartLoginFlow]
            }
            Routed::Login(LoginAction::CancelFlow) => {
                self.cancel_login_flow();
                Vec::new()
            }
            Routed::Picker(PickerAction::Choose {
                provider,
                instance,
                id,
            }) => {
                self.choose_model(provider, instance.as_deref(), &id);
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
            Routed::Profile(ProfileAction::Switch(name)) => {
                self.switch_profile(&name);
                Vec::new()
            }
            Routed::Profile(ProfileAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Resume(crate::screens::ResumeAction::Pick(path)) => {
                self.resume_session(&path);
                Vec::new()
            }
            Routed::Resume(crate::screens::ResumeAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Picker(PickerAction::Retry) => self.retry_fetch(),
            Routed::Login(LoginAction::Quit) => {
                // With a session behind the login, Esc means "back".
                let from_chat = matches!(
                    &self.screen,
                    Screen::Login(login) if login.purpose == LoginPurpose::FromChat
                );
                if from_chat {
                    self.screen = Screen::Chat;
                } else {
                    self.should_quit = true;
                }
                Vec::new()
            }
            Routed::Picker(PickerAction::None)
            | Routed::Profile(ProfileAction::None)
            | Routed::Resume(crate::screens::ResumeAction::None)
            | Routed::Login(LoginAction::None) => Vec::new(),
            Routed::Chat(chat_action) => self.on_chat_key(key, chat_action),
        }
    }

    /// Chat keys that are the app's, not the input box's.
    fn on_chat_key(&mut self, key: KeyEvent, action: ChatAction) -> Vec<Effect> {
        // Esc during a running turn cancels the turn, from anywhere —
        // approval modal included: the pending answer is released, the
        // rest declined, and the task drops the turn at its next await.
        // The y/N modal keys keep answering single calls.
        if self.running && key.code == KeyCode::Esc {
            self.cancel_turn();
            return Vec::new();
        }
        // The approval modal takes every key while it is up.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.answer_approval(true),
                KeyCode::Char('n' | 'N') => self.answer_approval(false),
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
        // Plain Ctrl combos only: with Alt also held the key is AltGr
        // composing a character, which the input already took.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            // Other Ctrl combos (including Ctrl-P mid-turn) are inert.
            if key.code == KeyCode::Char('p') && !self.running {
                return self.open_picker();
            }
            return Vec::new();
        }
        // Esc while entering a command cancels the entry instead of the
        // chat-level meanings (quit confirm / decline).
        if key.code == KeyCode::Esc && self.chat.input.starts_with('/') {
            self.chat.input.clear();
            self.chat.cursor = 0;
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            if !self.running {
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
                } else if text.trim().starts_with('/') {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.attachments.clear();
                    crate::commands::dispatch(self, text.trim())
                } else if self.provider.is_none() {
                    // The text stays: it is the prompt for once the
                    // profile has its key.
                    self.transcript
                        .push_notice("this profile has no API key; /login to add one");
                    Vec::new()
                } else {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.attachments.clear();
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

    /// The login screen validated a new instance's input: fetch the
    /// models; the fetch is the validation (a wrong key or a dead server
    /// both surface as the fetch's error).
    fn validate_login(&mut self) -> Vec<Effect> {
        let Screen::Login(login) = &self.screen else {
            return Vec::new();
        };
        let Some(pending) = login.pending.clone() else {
            return Vec::new();
        };
        let input = login.input.trim().to_owned();
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

    /// Esc on the flow stage: end the flow task and return to the
    /// instance list.
    fn cancel_login_flow(&mut self) {
        self.login_cancel = None;
        if let Screen::Login(login) = &mut self.screen {
            login.stage = crate::screens::LoginStage::Instances;
            login.status = LoginStatus::Idle;
            login.flow_instruction = None;
            login.flow_waiting = None;
        }
    }

    /// Enter on a logged-in instance: it becomes the profile's active
    /// one, the session moves to the login's tab, and the agent rebuilds
    /// carrying the conversation over — a picker when no model is stored
    /// yet, the chat otherwise.
    fn activate_instance(&mut self) -> Vec<Effect> {
        let Screen::Login(login) = &self.screen else {
            return Vec::new();
        };
        if self.running {
            self.transcript
                .push_notice("a turn is running — the switch waits for it to finish");
            return Vec::new();
        }
        let tab = login.profile.clone();
        let Some(name) = login.selected_instance().map(str::to_owned) else {
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
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(&tab));
        self.options.thinking = effective.thinking_effort().unwrap_or_default();
        let Some(backend) = Backend::from_profile(&effective) else {
            self.error = Some(format!(
                "no usable credentials for {} — /login first",
                instance.provider
            ));
            return Vec::new();
        };
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let label = format!("{} — {name}", backend.config_name());
        // The agent builds on the new provider, so it installs first.
        self.provider = Some(backend.provider());
        self.backend = Some(backend);
        match self.build_agent(session) {
            Ok(agent) => {
                self.session_profile = Some(tab);
                self.agent = Some(agent);
                if let Some(model) = effective.model {
                    self.screen = Screen::Chat;
                    self.transcript
                        .push_notice(&format!("switched to {label} · {model}"));
                    Vec::new()
                } else {
                    self.screen = Screen::Picker(PickerScreen {
                        status: PickerStatus::Loading,
                        ..PickerScreen::new(self.models.clone(), PickerPurpose::Initial)
                    });
                    vec![Effect::FetchModels(FetchWhy::Bootstrap)]
                }
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
            }
        }
    }

    /// `d` on a logged-in instance: forget it. When it was the session
    /// tab's active one, the backend re-resolves from what remains —
    /// another instance of the provider, or none.
    fn remove_instance(&mut self) -> Vec<Effect> {
        let Screen::Login(login) = &self.screen else {
            return Vec::new();
        };
        let tab = login.profile.clone();
        let Some(name) = login.selected_instance().map(str::to_owned) else {
            return Vec::new();
        };
        self.config.set_for_profile(Some(&tab), |settings| {
            settings.remove_instance(&name);
        });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        if self.session_profile.as_deref() == Some(tab.as_str()) {
            let effective = self.config.effective(Some(&tab));
            match Backend::from_profile(&effective) {
                Some(backend) => {
                    self.backend = Some(backend.clone());
                    self.provider = Some(backend.provider());
                }
                None => {
                    self.backend = None;
                    self.provider = None;
                    self.agent = None;
                }
            }
        }
        // Sync the screen's copy of the profile and keep the selection
        // inside the shrunken list.
        if let Screen::Login(login) = &mut self.screen {
            login.stored = self.config.effective(Some(&tab));
            let rows = login.instance_rows().len();
            if login.selected >= rows {
                login.selected = rows.saturating_sub(1);
            }
        }
        self.transcript.push_notice(&format!("forgot {name}"));
        Vec::new()
    }

    /// A step of the running login flow: render it on the login screen,
    /// and on completion validate the credential it produced through the
    /// same fetch path a typed key takes.
    fn on_login_update(&mut self, update: rhema_provider::LoginUpdate) -> Vec<Effect> {
        let Screen::Login(login) = &mut self.screen else {
            // The flow was left behind (Esc raced a completed update):
            // drop the credential with the screen it belonged to.
            self.login_cancel = None;
            return Vec::new();
        };
        match update {
            rhema_provider::LoginUpdate::Instruct(instruction) => {
                login.flow_instruction = Some(instruction);
                login.flow_waiting = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Waiting(text) => {
                login.flow_waiting = Some(text);
                Vec::new()
            }
            rhema_provider::LoginUpdate::Failed(message) => {
                login.status = LoginStatus::Error(message);
                // The flow task has ended; the handle can go.
                self.login_cancel = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Completed(token) => {
                login.flow_waiting = Some("fetching the model list".to_owned());
                self.login_cancel = None;
                // The flow's instance was named before the flow started.
                let instance = login.pending.as_ref().map(|p| p.name.clone());
                vec![Effect::FetchModels(FetchWhy::Login {
                    backend: Backend::Copilot { api_key: token },
                    instance,
                })]
            }
        }
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
    pub fn open_picker(&mut self) -> Vec<Effect> {
        // No backend, no list to show: the runner would silently drop
        // the fetch and the picker would spin on Loading forever.
        if self.backend.is_none() {
            self.transcript
                .push_notice("no provider on this profile; /login first");
            return Vec::new();
        }
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
                self.track_phase(&owned);
                self.transcript.apply(&owned);
                Vec::new()
            }
            UiEvent::ModelsLoaded { why, result } => {
                self.on_models_loaded(why, result);
                Vec::new()
            }
            UiEvent::LoginUpdate(update) => self.on_login_update(update),
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
            UiEvent::TurnCancelled { agent } => {
                self.cancel_finished(agent);
                Vec::new()
            }
        }
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    fn on_models_loaded(&mut self, why: FetchWhy, result: Vec<crate::events::CatalogFetch>) {
        match why {
            FetchWhy::Login { backend, instance } => {
                let Screen::Login(login) = &mut self.screen else {
                    return;
                };
                // A login opened from the chat lands in a picker that also
                // knows the way back.
                let from_chat = login.purpose == LoginPurpose::FromChat;
                let tab = login.profile.clone();
                // Every login writes a named instance; a lost name (a
                // flow that outlived its screen) falls back to the
                // provider's own.
                let instance = instance.unwrap_or_else(|| backend.config_name().to_owned());
                // The login fetch validates one provider: its result is
                // the catalog's only entry.
                match result.into_iter().next().map(|entry| entry.result) {
                    Some(Ok(models)) => {
                        let provider = backend.config_name();
                        let (api_key, base_url) = match &backend {
                            Backend::LlamaCpp { url, api_key } => {
                                (api_key.clone(), Some(url.clone()))
                            }
                            Backend::OpenAi { api_key, base_url } => {
                                (Some(api_key.clone()), base_url.clone())
                            }
                            Backend::Google { api_key }
                            | Backend::OpenRouter { api_key }
                            | Backend::Anthropic { api_key }
                            | Backend::DeepSeek { api_key }
                            | Backend::Copilot { api_key } => (Some(api_key.clone()), None),
                        };
                        self.config.set_for_profile(Some(&tab), |settings| {
                            settings.provider = Some(provider.to_owned());
                            settings.instance = Some(instance.clone());
                            settings.upsert_instance(
                                &instance,
                                rhema_config::ProviderInstance {
                                    provider: provider.to_owned(),
                                    api_key,
                                    base_url,
                                },
                            );
                        });
                        if let Err(err) = self.store.save(&self.config) {
                            login.status = LoginStatus::Error(format!("cannot save config: {err}"));
                            return;
                        }
                        self.session_profile = Some(tab);
                        self.backend = Some(backend.clone());
                        self.provider = Some(backend.provider());
                        // The other logins' catalogs stay: a fresh login
                        // must not blank the picker down to one provider.
                        // The instance's own slice replaces itself, so a
                        // stale copy cannot linger — a provider may hold
                        // several.
                        self.models.retain(|entry| {
                            entry.provider != provider
                                || entry.instance.as_deref() != Some(instance.as_str())
                        });
                        self.models.extend(models.iter().map(|model| {
                            crate::screens::CatalogModel {
                                provider,
                                instance: Some(instance.clone()),
                                model: model.clone(),
                            }
                        }));
                        self.models.sort_by(|a, b| {
                            a.provider
                                .cmp(b.provider)
                                .then(a.instance.cmp(&b.instance))
                                .then(a.model.id.cmp(&b.model.id))
                        });
                        // A single-model server (plain llama-server rather
                        // than a router) has nothing to pick: take it and
                        // go straight to chat.
                        if models.len() == 1 {
                            let id = models[0].id.clone();
                            self.choose_model(provider, Some(&instance), &id);
                            return;
                        }
                        let purpose = if from_chat {
                            PickerPurpose::FromChat
                        } else {
                            PickerPurpose::Initial
                        };
                        self.screen =
                            Screen::Picker(PickerScreen::new(self.models.clone(), purpose));
                    }
                    Some(Err(message)) => login.on_validated(&Err(message)),
                    // The dispatch always spawns one backend per login;
                    // an empty result cannot arrive.
                    None => login.on_validated(&Err("no provider answered".to_owned())),
                }
            }
            FetchWhy::Bootstrap | FetchWhy::Refresh => {
                // One result per queried backend: successes merge into the
                // catalog; a total failure is the picker's error, and a
                // partial one is named in its footer — a missing provider
                // must never look like an empty one.
                let (catalogs, failed): (Vec<_>, Vec<_>) =
                    result.into_iter().partition(|entry| entry.result.is_ok());
                let models: Vec<crate::screens::CatalogModel> = catalogs
                    .iter()
                    .flat_map(|entry| {
                        let models = entry.result.as_deref().unwrap_or_default();
                        models
                            .iter()
                            .map(move |model| crate::screens::CatalogModel {
                                provider: entry.provider,
                                instance: entry.instance.clone(),
                                model: model.clone(),
                            })
                    })
                    .collect();
                self.models = models;
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
                        // The single-model failover applies at startup
                        // too: a server with one offering has nothing
                        // to pick. Refresh (ctrl-p) always shows the
                        // list instead.
                        if picker.purpose == PickerPurpose::Initial && picker.models.len() == 1 {
                            // Nothing follows in this arm: the chat
                            // screen choose_model installed stands.
                            let entry = picker.models[0].clone();
                            self.choose_model(
                                entry.provider,
                                entry.instance.as_deref(),
                                &entry.model.id,
                            );
                        }
                    }
                }
            }
        }
    }

    /// A model was named for `/model <needle>`: resolve the needle against
    /// the cached catalog — exact id first, then a unique prefix — and
    /// switch, or say what went missing. `/model` with no needle opens
    /// the full picker instead.
    pub fn choose_model_by_id(&mut self, needle: &str) {
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
            None if exact_count > 1 || prefix_count > 1 => self.push_notice(&format!(
                "{} models match {needle:?}; name one fully or use /model",
                exact_count.max(prefix_count)
            )),
            None => self.push_notice(&format!(
                "no model matches {needle:?} in the cached catalog; /model lists them"
            )),
        }
    }

    /// A model was chosen in the picker: persist it and (re)build the
    /// agent, carrying the current session over. The old agent is only
    /// dropped once the new one built, so a rebuild failure cannot strand
    /// the chat.
    fn choose_model(&mut self, provider: &'static str, instance: Option<&str>, id: &str) {
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
            self.error = Some(format!("cannot save config: {err}"));
            return;
        }
        let backend = match Backend::from_profile(&self.effective()) {
            Some(backend) => backend,
            None => {
                self.error = Some(format!(
                    "no usable credentials for {provider}; /login first"
                ));
                return;
            }
        };
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        match self.build_agent(session) {
            Ok(agent) => {
                // An interactive switch — the picker opened from the chat,
                // or a `/model <needle>` typed there — says where it
                // landed; the login and startup auto-selects stay quiet.
                let from_chat = matches!(&self.screen, Screen::Picker(picker) if picker.purpose == PickerPurpose::FromChat);
                if from_chat || matches!(self.screen, Screen::Chat) {
                    self.transcript
                        .push_notice(&format!("model switched to {provider}/{id}"));
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
        let agent_config = agent_config(&self.options, self.effective().model.clone());
        CodingAgent::from_session(provider, agent_config, session)
    }

    /// The runner accepted a turn: wire the approval answer channel and
    /// start in the thinking phase.
    pub fn begin_turn(&mut self, answers: Option<ApprovalAnswerer>) {
        self.answers = answers;
        self.processing = false;
        self.thinking.clear();
        self.remember_profile();
    }

    /// Records the session's profile as the last one this workspace
    /// chose or ran with — an explicit `/profile` switch or a turn's
    /// start — so the next launch here opens on it. Written only on
    /// change; a failure surfaces in the status strip like every other
    /// config write — neither the switch nor the turn is blocked by it.
    fn remember_profile(&mut self) {
        let Some(profile) = self.session_profile.clone() else {
            return;
        };
        let workspace = self.options.workspace.clone();
        if self.config.last_profile_in(&workspace).as_deref() == Some(profile.as_str()) {
            return;
        }
        self.config.note_profile_in(&workspace, &profile);
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Tracks which phase a running turn is in, from the engine's own
    /// events: streaming is thinking, a tool call is processing. A result
    /// leaves the phase alone — the next call or step moves it, so the
    /// label never flickers between back-to-back commands. Reasoning
    /// deltas fill the preview buffer; real output or a tool call clears
    /// it — the thinking block vanishes once the answer starts.
    fn track_phase(&mut self, owned: &OwnedEvent) {
        match owned {
            OwnedEvent::StepStart { .. } | OwnedEvent::TextDelta(_) => {
                self.processing = false;
                self.thinking.clear();
            }
            OwnedEvent::ThinkingDelta(delta) => self.push_thinking(delta),
            // A tool call means a command runs. Its arguments often never
            // stream as deltas, so the label stays phase-only.
            OwnedEvent::ToolCallStart { .. } => {
                self.processing = true;
                self.thinking.clear();
            }
            _ => {}
        }
    }

    /// Appends one reasoning piece to the preview buffer, keeping only
    /// the tail: the status strip shows at most a few wrapped rows, so
    /// older reasoning has no screen to live on.
    fn push_thinking(&mut self, delta: &str) {
        const TAIL_CAP: usize = 2048;
        self.thinking.push_str(delta);
        if self.thinking.len() > TAIL_CAP {
            let mut cut = self.thinking.len() - TAIL_CAP;
            while !self.thinking.is_char_boundary(cut) {
                cut += 1;
            }
            self.thinking.drain(..cut);
        }
    }

    /// Answers the modal, if the approval hook is listening.
    fn answer_approval(&mut self, approved: bool) {
        self.send_answer(approved);
        self.pending_approval = None;
    }

    fn send_answer(&self, approved: bool) {
        if let Some(answers) = &self.answers {
            answers.answer(approved);
        }
    }

    /// Cancels the running turn from the keyboard. The approval bridge
    /// blocks its thread on the answer, so a pending modal is released
    /// first; upcoming approvals are declined; then the flag flips and
    /// the turn task drops the future at its current await. The agent
    /// comes back in a `TurnCancelled`.
    fn cancel_turn(&mut self) {
        self.answer_approval(false);
        self.decline_all = true;
        if let Some(cancel) = &self.turn_cancel {
            let _ = cancel.send(true);
        }
    }

    /// The turn task was cancelled from the keyboard: take the agent
    /// back, drop the turn's state, and mark the cutoff in the transcript.
    fn cancel_finished(&mut self, agent: Box<CodingAgent>) {
        self.agent = Some(*agent);
        self.running = false;
        self.turn_started = None;
        self.turn_cancel = None;
        self.processing = false;
        self.thinking.clear();
        self.decline_all = false;
        self.pending_approval = None;
        self.answers = None;
        self.transcript.push_notice("turn cancelled");
    }

    /// The turn task finished: take the agent back, record the usage or
    /// the error, and drop any stale modal (the turn cannot finish while
    /// an approval is unanswered, so a leftover modal means the answer
    /// channel is gone).
    fn end_turn(&mut self, agent: Box<CodingAgent>, result: Result<ModelOutput, String>) {
        self.agent = Some(*agent);
        self.running = false;
        self.turn_started = None;
        self.turn_cancel = None;
        self.processing = false;
        self.thinking.clear();
        self.decline_all = false;
        self.pending_approval = None;
        self.answers = None;
        match result {
            Ok(output) => {
                if let Some(pricing) = self.current_model_pricing() {
                    let usage = output.usage;
                    // Cached prompt tokens bill at a fraction of base
                    // input everywhere — Anthropic reads ~10%, OpenAI
                    // 50%, DeepSeek and Gemini ~25%; the cross-provider
                    // middle stands in where no exact rate is published.
                    // Cache writes ride the full input bucket.
                    const CACHED_INPUT_FALLBACK: f64 = 0.25;
                    let cached_rate = pricing
                        .cached_input_per_mtok
                        .unwrap_or(pricing.input_per_mtok * CACHED_INPUT_FALLBACK);
                    let uncached = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
                    self.session_cost += uncached as f64 / 1_000_000.0 * pricing.input_per_mtok
                        + usage.cached_input_tokens as f64 / 1_000_000.0 * cached_rate
                        + usage.output_tokens as f64 / 1_000_000.0 * pricing.output_per_mtok;
                }
                self.usage = Some(output.usage);
            }
            Err(message) => self.error = Some(message),
        }
    }

    /// The settings that apply to this session's profile: field-wise, the
    /// named profile (or the one chosen at startup) over the top-level
    /// section. Computed on demand — it changes mid-session.
    pub fn effective(&self) -> rhema_config::Profile {
        self.config.effective(self.session_profile.as_deref())
    }

    /// The profiles the input palette completes `/profile` with: the
    /// config registry's rows, the session's own marked.
    pub fn palette_profiles(&self) -> Vec<crate::screens::PaletteProfile> {
        self.config
            .profiles
            .iter()
            .map(|(name, profile)| crate::screens::PaletteProfile {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                active: self.session_profile.as_deref() == Some(name.as_str()),
            })
            .collect()
    }

    /// The active profile's name and display color: the `--profile` flag
    /// over the stored active profile, colored by its config (a `#RRGGBB`
    /// string) or by a name-based theme tone.
    pub fn active_profile(&self) -> Option<(String, ratatui::style::Color)> {
        let name = self.session_profile.clone()?;
        let custom = self
            .config
            .profiles
            .get(&name)
            .and_then(|profile| profile.color.as_deref())
            .and_then(crate::palette::parse_hex_color);
        let color =
            custom.unwrap_or_else(|| crate::palette::default_profile_color(&name, self.theme));
        Some((name, color))
    }

    /// The pricing of the model the session currently runs on, when its
    /// adapter prices it.
    fn current_model_pricing(&self) -> Option<rhema_provider::Pricing> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.pricing)
    }

    /// The context window of the current model, when known.
    pub fn current_context_window(&self) -> Option<u64> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.input_token_limit)
    }

    /// The context footprint the footer shows: the last turn's reported
    /// input tokens when one exists, otherwise an estimate over the
    /// loaded history — a resumed session has already spent context
    /// before its first turn here, and an empty one estimates to zero.
    pub fn context_used(&self) -> u64 {
        match self.usage {
            Some(usage) => u64::from(usage.input_tokens),
            None => estimate_history_tokens(&self.live_session().history),
        }
    }

    /// The session the conversation currently lives in: the agent's when
    /// one is built, the bare one otherwise.
    fn live_session(&self) -> &Session {
        match &self.agent {
            Some(agent) => agent.session(),
            None => &self.session,
        }
    }
}

/// The coding-agent config implied by the startup flags and the chosen
/// model.
fn agent_config(options: &RunOptions, model: Option<String>) -> CodingAgentConfig {
    CodingAgentConfig {
        max_turn_steps: options.max_steps,
        thinking: options.thinking,
        base_system: options.base_system.clone(),
        ..CodingAgentConfig::new(model.unwrap_or_default(), &options.workspace)
    }
}

/// A rough token estimate over a history: about four characters per
/// token, counted across text, tool arguments, and tool results. There
/// is no tokenizer this side of the wire, and the footer is a gauge,
/// not an invoice.
fn estimate_history_tokens(history: &[Message]) -> u64 {
    let chars: usize = history
        .iter()
        .flat_map(|message| message.content.iter())
        .map(|content| match content {
            Content::Text(text) => text.len(),
            Content::ToolCall(call) => call.args.to_string().len(),
            Content::ToolResult(result) => result.content.len(),
        })
        .sum();
    (chars as u64) / 4
}

/// The profile a session runs on when no `--profile` names one: the
/// profile this workspace last served a turn under while it still can,
/// else the default profile while it can serve turns, otherwise the
/// first stored profile that can (the registry's order) — a logged-in
/// profile must not open the login screen. Isolation still holds: this
/// only picks the section up front, and nothing reads across profiles
/// afterwards. An explicit flag never reaches here.
pub(crate) fn startup_profile(config: &rhema_config::Config, workspace: &Path) -> String {
    let default = "personal";
    let ready = |name: &str| {
        config
            .profiles
            .get(name)
            .map(|profile| Backend::from_profile(profile).is_some())
            .unwrap_or(false)
    };
    if let Some(last) = config.last_profile_in(workspace).filter(|name| ready(name)) {
        return last;
    }
    if ready(default) {
        return default.to_owned();
    }
    config
        .profiles
        .keys()
        .find(|name| ready(name))
        .cloned()
        .unwrap_or_else(|| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::TuiApproval;
    use crate::events::OwnedEvent;
    use crate::screens::LoginProvider;
    use crate::store::MemoryStore;
    use crate::transcript::Line;
    use rhema_coding_agent::{Approval, CodingAgentConfig};
    use rhema_core::{Message, Role, Session, StopReason, ToolCall, Usage};
    use rhema_provider::ModelCard;
    use rhema_provider_google::GoogleProvider;
    use std::collections::BTreeMap;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn model(id: &str) -> ModelCard {
        ModelCard {
            id: id.into(),
            display_name: id.into(),
            description: String::new(),
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            generation_methods: vec!["generateContent".into()],
            pricing: None,
        }
    }

    fn models() -> Vec<ModelCard> {
        vec![model("m-b"), model("m-a")]
    }

    /// A `ModelsLoaded` result of one provider's catalog, the shape the
    /// fetch task reports per queried backend. The instance is named
    /// after the provider, the way a migrated login lands.
    fn loaded(provider: &'static str, models: Vec<ModelCard>) -> Vec<crate::events::CatalogFetch> {
        vec![crate::events::CatalogFetch {
            provider,
            instance: Some(provider.to_owned()),
            result: Ok(models),
        }]
    }

    /// A `ModelsLoaded` result of one provider's failure.
    fn failed(provider: &'static str, message: &str) -> Vec<crate::events::CatalogFetch> {
        vec![crate::events::CatalogFetch {
            provider,
            instance: Some(provider.to_owned()),
            result: Err(message.to_owned()),
        }]
    }

    /// A priced card, for cost-math tests: the price rides the row, the
    /// way the adapters hand catalogs out.
    fn priced_model(id: &str, pricing: rhema_provider::Pricing) -> ModelCard {
        ModelCard {
            pricing: Some(pricing),
            ..model(id)
        }
    }

    /// A llama.cpp row: an id with none of the metadata Google reports.
    fn llama_model(id: &str) -> ModelCard {
        ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }

    fn test_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "ls" }),
            provider_data: serde_json::Value::Null,
        }
    }

    struct Harness {
        app: App,
        store: Arc<MemoryStore>,
    }

    /// Stores `key` as `provider`'s instance on `profile` — the login
    /// flow's write shape.
    fn store_key(profile: &mut rhema_config::Profile, provider: &str, key: &str) {
        profile.upsert_instance(
            provider,
            rhema_config::ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some(key.to_owned()),
                base_url: None,
            },
        );
    }

    /// A profile serving turns on `provider` through one stored instance
    /// named after it.
    fn keyed_profile(provider: &str, key: &str) -> rhema_config::Profile {
        let mut profile = rhema_config::Profile {
            provider: Some(provider.to_owned()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut profile, provider, key);
        profile
    }

    fn harness(api_key: Option<&str>, model: Option<&str>, session: Session) -> Harness {
        let mut personal = rhema_config::Profile {
            // The key alone never served a turn: a usable
            // profile names its provider too.
            provider: Some("google".into()),
            model: model.map(str::to_owned),
            ..rhema_config::Profile::default()
        };
        if let Some(key) = api_key {
            personal.upsert_instance(
                "google",
                rhema_config::ProviderInstance {
                    provider: "google".into(),
                    api_key: Some(key.to_owned()),
                    base_url: None,
                },
            );
        }
        let config = Config {
            profiles: BTreeMap::from([("personal".to_owned(), personal)]),
            ..Default::default()
        };
        let store = Arc::new(MemoryStore::new(config.clone()));
        let backend: Option<Backend> = api_key.map(|key| Backend::Google {
            api_key: key.to_owned(),
        });
        let app = App::new(AppSetup {
            store: store.clone(),
            config,
            backend,
            session,
            options: RunOptions {
                theme: crate::args::ThemeKind::Dark,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some("personal".into()),
                workspace: PathBuf::from("."),
                max_steps: 4,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        Harness { app, store }
    }

    /// Drives the login screen's add flow to the credential stage for a
    /// provider sitting at `row` on the provider list, accepting the
    /// suggested instance name — ready to type the credential.
    fn descend_to_credential(h: &mut Harness, row: usize) -> String {
        use crate::screens::LoginStage;
        // Down to the add row of the instance list, then enter.
        let Screen::Login(login) = &h.app.screen else {
            panic!("expected the login screen");
        };
        for _ in 0..login.instance_rows().len().saturating_sub(1) {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        for _ in 0..row {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Login(login) = &h.app.screen else {
            panic!("expected the login screen");
        };
        let name = login.input.clone();
        assert_eq!(login.stage, LoginStage::NameInstance);
        // Enter accepts the suggested name; copilot's flow starts
        // instead of asking for a key, which its own tests drive.
        h.app.on_key(key(KeyCode::Enter));
        name
    }

    #[test]
    fn a_paste_on_the_login_screen_fills_the_credential() {
        let mut h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Login(_)));
        // Descend into the credential stage first.
        descend_to_credential(&mut h, 0);
        h.app.on_paste("sk-123");
        match &h.app.screen {
            Screen::Login(login) => assert_eq!(login.input, "sk-123"),
            _ => panic!("expected the login screen"),
        }
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
            result: loaded("google", models()),
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
        descend_to_credential(&mut h, 0);
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
                instance: Some("google".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.models.len(), 2),
            _ => panic!("expected the picker after a successful login"),
        }
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.instance.as_deref(), Some("google"));
        assert_eq!(
            effective.instances["google"].api_key.as_deref(),
            Some("secret")
        );
        assert_eq!(effective.google_api_key().as_deref(), Some("secret"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::Google { ref api_key }) if api_key == "secret"
        ));
    }

    #[test]
    fn a_login_merges_its_catalog_instead_of_replacing_the_cache() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        // /login, then the openrouter row and a key.
        for c in "/login".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        descend_to_credential(&mut h, 2);
        for c in "o".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenRouter { .. },
                ..
            })]
        ));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenRouter {
                    api_key: "o".into(),
                },
                instance: Some("openrouter".into()),
            },
            result: loaded(
                "openrouter",
                vec![llama_model("openrouter/auto"), llama_model("z-ai/glm-5.3")],
            ),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.models.len(), 4, "google stays: {picker:?}");
                let providers: Vec<&str> = picker.models.iter().map(|m| m.provider).collect();
                assert_eq!(providers, ["google", "google", "openrouter", "openrouter"]);
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn llama_login_validates_by_server_url_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // The llama.cpp row is second on the add flow's provider list.
        let name = descend_to_credential(&mut h, 1);
        assert_eq!(name, "llama-cpp");
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(
                    login.pending.as_ref().map(|p| p.provider),
                    Some(crate::screens::LoginProvider::LlamaCpp)
                );
                assert_eq!(login.input_label(), "server URL: ");
            }
            _ => panic!("expected the login screen"),
        }
        for c in "http://box:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        // Enter captures the URL; the bearer key prompt follows, and a
        // blank one is accepted (an open server).
        h.app.on_key(key(KeyCode::Enter));
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.input_label(), "API key: ");
            }
            _ => panic!("expected the bearer-key prompt"),
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            },
            result: loaded(
                "llama-cpp",
                vec![llama_model("qwen3-8b"), llama_model("qwen3-32b")],
            ),
        });
        // Router mode: several models land in the picker with no
        // metadata, and the rows carry the llama.cpp prefix — not google's.
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(picker.models.iter().all(|m| m.provider == "llama-cpp"));
                assert_eq!(picker.models.len(), 2);
                let ids: Vec<&str> = picker.models.iter().map(|m| m.model.id.as_str()).collect();
                assert_eq!(ids, ["qwen3-32b", "qwen3-8b"]);
                assert!(picker.models[0].model.input_token_limit.is_none());
            }
            _ => panic!("expected the picker after a successful llama login"),
        }
        let config = h.store.load().unwrap();
        let effective = config.effective(None);
        assert_eq!(effective.provider.as_deref(), Some("llama-cpp"));
        assert_eq!(
            effective.instances["llama-cpp"].base_url.as_deref(),
            Some("http://box:8080")
        );
        assert_eq!(effective.llama_cpp_url(), "http://box:8080");
    }

    #[test]
    fn anthropic_login_validates_by_key_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // Anthropic is the fourth provider row.
        let name = descend_to_credential(&mut h, 3);
        assert_eq!(name, "anthropic");
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.input_label(), "API key: ");
            }
            _ => panic!("expected the login screen"),
        }
        for c in "sk-ant-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Anthropic {
                    api_key: "sk-ant-01".into(),
                },
                instance: Some("anthropic".into()),
            })]
        );
        // The model-list fetch is the validation; success lands in the
        // picker prefixed `anthropic/`, and the key lands in the profile.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Anthropic {
                    api_key: "sk-ant-01".into(),
                },
                instance: Some("anthropic".into()),
            },
            result: loaded(
                "anthropic",
                vec![
                    llama_model("claude-sonnet-4-5"),
                    llama_model("claude-opus-4-1"),
                ],
            ),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(picker.models.iter().all(|m| m.provider == "anthropic"));
                assert_eq!(picker.models.len(), 2);
                // The Anthropic listing carries identity only: no
                // context, no pricing.
                assert!(picker.models[0].model.input_token_limit.is_none());
                assert!(picker.models[0].model.pricing.is_none());
            }
            _ => panic!("expected the picker after a successful anthropic login"),
        }
        let config = h.store.load().unwrap();
        let effective = config.effective(None);
        assert_eq!(effective.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            effective.instances["anthropic"].api_key.as_deref(),
            Some("sk-ant-01")
        );
        assert_eq!(effective.anthropic_api_key().as_deref(), Some("sk-ant-01"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::Anthropic { ref api_key }) if api_key == "sk-ant-01"
        ));
    }

    #[test]
    fn openai_login_validates_by_key_with_the_default_base() {
        let mut h = harness(None, None, Session::default());
        // OpenAI is the fifth provider row.
        descend_to_credential(&mut h, 4);
        // The base URL prompt comes first: an empty one means OpenAI
        // itself.
        h.app.on_key(key(KeyCode::Enter));
        for c in "sk-proj-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-proj-01".into(),
                    base_url: None,
                },
                instance: Some("openai".into()),
            })]
        );
        // A failed validation stays on the login screen and persists
        // nothing — same contract as the other keyed providers.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-proj-01".into(),
                    base_url: None,
                },
                instance: Some("openai".into()),
            },
            result: failed("openai", "openai-compatible api error (HTTP 401): denied"),
        });
        match &h.app.screen {
            Screen::Login(login) => assert!(matches!(login.status, LoginStatus::Error(_))),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.app.backend.is_none());
        assert!(h.store.load().unwrap().effective(None).instances.is_empty());
    }

    #[test]
    fn anthropic_and_openai_profiles_build_their_own_backends() {
        let keyed = |provider: &str, key: Option<&str>| {
            let mut profile = rhema_config::Profile {
                provider: Some(provider.to_owned()),
                ..rhema_config::Profile::default()
            };
            if let Some(key) = key {
                store_key(&mut profile, provider, key);
            }
            profile
        };
        let backend = Backend::from_profile(&keyed("anthropic", Some("k-a")))
            .expect("a keyed anthropic profile serves turns");
        assert_eq!(backend.config_name(), "anthropic");
        assert!(
            matches!(backend, Backend::Anthropic { ref api_key } if api_key == "k-a"),
            "the anthropic profile builds the anthropic backend: {backend:?}"
        );
        let backend = Backend::from_profile(&keyed("openai", Some("k-o")))
            .expect("a keyed openai profile serves turns");
        assert_eq!(backend.config_name(), "openai");
        assert!(
            matches!(backend, Backend::OpenAi { ref api_key, .. } if api_key == "k-o"),
            "the openai profile builds the openai backend: {backend:?}"
        );
        // No key, no backend — a keyless profile cannot serve turns.
        let mut cross = rhema_config::Profile {
            provider: Some("anthropic".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut cross, "openai", "k-o");
        assert_eq!(
            Backend::from_profile(&cross),
            None,
            "an anthropic profile is not served by an openai key"
        );
        assert_eq!(
            Backend::from_profile(&rhema_config::Profile {
                provider: Some("openai".into()),
                ..rhema_config::Profile::default()
            }),
            None
        );
        // No provider named, and a spelling no provider answers to, both
        // leave the backend unresolved: the login flow runs, nothing
        // silently serves google.
        assert_eq!(
            Backend::from_profile(&rhema_config::Profile::default()),
            None,
            "a provider-less profile opens the login flow"
        );
        let mut nonsense = rhema_config::Profile {
            provider: Some("nonsense".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut nonsense, "google", "k");
        assert_eq!(
            Backend::from_profile(&nonsense),
            None,
            "an unknown spelling never falls back to google"
        );
    }

    #[test]
    fn candidates_span_every_provider_the_profile_holds_keys_for() {
        let mut profile = keyed_profile("openrouter", "o");
        store_key(&mut profile, "google", "g");
        let names: Vec<&str> = Backend::candidates_from_profile(&profile)
            .iter()
            .map(|candidate| candidate.backend.config_name())
            .collect();
        assert_eq!(names, ["openrouter", "google"], "the active provider leads");

        // llama.cpp joins only as the active provider: a server address
        // is a login, not a catalog to browse.
        profile.provider = Some("llama-cpp".into());
        let names: Vec<&str> = Backend::candidates_from_profile(&profile)
            .iter()
            .map(|candidate| candidate.backend.config_name())
            .collect();
        assert_eq!(names, ["llama-cpp", "google", "openrouter"]);
    }

    #[test]
    fn candidates_name_every_stored_instance_and_put_the_active_one_first() {
        let profile = rhema_config::Profile {
            provider: Some("llama-cpp".to_owned()),
            instance: Some("gpu box".to_owned()),
            instances: BTreeMap::from([
                (
                    "gpu box".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        base_url: Some("http://gpu:8080".into()),
                    },
                ),
                (
                    "home".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        base_url: Some("http://home:8080".into()),
                    },
                ),
                (
                    "work sub".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "copilot".into(),
                        api_key: Some("gho-1".into()),
                        base_url: None,
                    },
                ),
            ]),
            ..rhema_config::Profile::default()
        };
        let candidates = Backend::candidates_from_profile(&profile);
        let rows: Vec<(Option<&str>, &str)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.instance.as_deref(),
                    candidate.backend.config_name(),
                )
            })
            .collect();
        // The active instance leads, the rest follow in name order —
        // every llama.cpp server joins, a stored URL being an explicit
        // login now.
        assert_eq!(
            rows,
            [
                (Some("gpu box"), "llama-cpp"),
                (Some("home"), "llama-cpp"),
                (Some("work sub"), "copilot"),
            ]
        );
        assert!(
            matches!(
                candidates[0].backend,
                Backend::LlamaCpp { ref url, .. } if url == "http://gpu:8080"
            ),
            "the active instance's own URL serves: {:?}",
            candidates[0].backend
        );
    }

    #[test]
    fn refresh_merges_every_providers_catalog_and_names_the_failures() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: [
                loaded("google", models()),
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
            ]
            .concat(),
        });
        h.app.on_key(ctrl('p'));
        // A refresh where google's catalog fails: the openrouter one keeps
        // the picker alive, and the footer names the gap.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: [
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
                failed("google", "gemini api error (HTTP 403): denied"),
            ]
            .concat(),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.status, PickerStatus::Ready);
                assert_eq!(picker.failed, ["google"]);
                assert_eq!(picker.models.len(), 1);
                assert_eq!(picker.models[0].provider, "openrouter");
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn a_total_catalog_failure_is_the_pickers_error() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.on_key(ctrl('p'));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: failed("google", "gemini api error (HTTP 403): denied"),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(matches!(picker.status, PickerStatus::Error(_)));
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn slash_model_with_a_needle_switches_directly() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        for c in "/model m-b".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(
            effects.is_empty(),
            "a named model rebuilds inline: {effects:?}"
        );
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-b")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("model switched to google/m-b".into()))
        );
    }

    #[test]
    fn slash_model_with_a_unique_prefix_resolves_and_an_unknown_needle_says_so() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        for c in "/model m-".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(h.app.screen, Screen::Chat),
            "the picker never opens"
        );
        assert!(
            h.app
                .transcript
                .lines()
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("2 models match"))),
            "ambiguous prefix says so"
        );
        for c in "/model nope".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            h.app.transcript.lines().iter().any(
                |line| matches!(line, Line::Notice(text) if text.contains("no model matches"))
            )
        );
    }

    #[test]
    fn a_resumed_session_continues_its_cost() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "earlier work"));
        session.cost_usd = 2.5;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        crate::sessions::write_session(&path, &session).unwrap();

        h.app.resume_session(&path);
        assert_eq!(h.app.session_cost, 2.5, "the footer continues the story");
        // A turn on top of it accumulates further, and the exit save
        // carries the total back into the file.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.model = Some("m-a".into())
        });
        let (exported, origin) = h.app.export_session().unwrap();
        assert_eq!(origin.as_deref(), Some(path.as_path()));
        assert_eq!(exported.cost_usd, 2.5);
    }

    #[test]
    fn slash_profile_switches_to_a_named_profile() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            store_key(settings, "openrouter", "o");
        });
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "o"));
        for c in "/profile work".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
        ));
    }

    #[test]
    fn choosing_from_another_provider_switches_the_backend() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            store_key(settings, "openrouter", "o");
        });
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: [
                loaded("google", models()),
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
            ]
            .concat(),
        });
        // Filter by provider, pick the openrouter model.
        h.app.on_key(ctrl('p'));
        for c in "openrouter".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "a pick rebuilds inline: {effects:?}");
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.backend,
            Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
        ));
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.provider.as_deref(), Some("openrouter"));
        assert_eq!(effective.model.as_deref(), Some("openrouter/auto"));
        assert!(h.app.transcript.lines().contains(&Line::Notice(
            "model switched to openrouter/openrouter/auto".into()
        )));
    }

    #[test]
    fn turn_costs_accumulate_from_the_model_pricing() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        // models() carries a 1M-token limit; give the fixture a price
        // directly, the way google's catalog bakes one in. The session
        // carries a message so the export has something to save.
        let mut h = harness(
            Some("k"),
            None,
            Session {
                history: vec![Message::text(Role::User, "earlier work")],
                driver_state: serde_json::Value::Null,
                cost_usd: 0.0,
            },
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            // $0.75/M in, $3.75/M out, cached input at a quarter — the
            // shape the google adapter bakes for a flash model.
            result: loaded(
                "google",
                vec![priced_model(
                    "gemini-3.8-flash",
                    rhema_provider::Pricing {
                        input_per_mtok: 0.75,
                        output_per_mtok: 3.75,
                        cached_input_per_mtok: Some(0.1875),
                    },
                )],
            ),
        });
        h.app.on_key(key(KeyCode::Char('a')));
        let effects = h.app.on_key(key(KeyCode::Enter));
        match effects.as_slice() {
            [Effect::StartTurn(_)] => {}
            other => panic!("expected a turn start, got {other:?}"),
        }
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    cached_input_tokens: 400_000,
                },
            }),
        });
        // 600k uncached in at $0.75/M + 400k cached in at a quarter of
        // that + 1M out at $3.75/M.
        assert!((h.app.session_cost - 4.275).abs() < 1e-9);
        // The exit save carries the total, so a resume continues it.
        let (exported, _) = h.app.export_session().unwrap();
        assert!((exported.cost_usd - 4.275).abs() < 1e-9);
    }

    #[test]
    fn a_single_model_server_auto_selects_and_skips_the_picker() {
        let mut h = harness(None, None, Session::default());
        // The llama.cpp row, the suggested name, the URL, and a blank
        // bearer key.
        descend_to_credential(&mut h, 1);
        for c in "http://localhost:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://localhost:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            },
            result: loaded("llama-cpp", vec![llama_model("qwen3-8b")]),
        });
        // Exactly one model: no picker, straight to chat on that model.
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("qwen3-8b")
        );
    }

    #[test]
    fn enter_on_a_stored_instance_activates_it() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("llama-cpp".into());
            settings.instance = Some("home".into());
            settings.model = Some("qwen3-8b".into());
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
            settings.upsert_instance(
                "gpu box",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    base_url: Some("http://gpu:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "gpu box" sorts before "home": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(effects.is_empty(), "a stored model goes straight to chat");
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(
            matches!(&h.app.backend, Some(Backend::LlamaCpp { url, .. }) if url == "http://gpu:8080"),
            "the activated instance's own URL serves: {:?}",
            h.app.backend
        );
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.instance.as_deref(), Some("gpu box"));
        assert_eq!(
            effective.model.as_deref(),
            Some("qwen3-8b"),
            "the same provider keeps its stored model"
        );
    }

    #[test]
    fn activating_across_providers_drops_the_foreign_model() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("openai".into());
            settings.instance = Some("openai".into());
            settings.model = Some("gpt-5".into());
            settings.upsert_instance(
                "openai",
                rhema_config::ProviderInstance {
                    provider: "openai".into(),
                    api_key: Some("sk".into()),
                    base_url: None,
                },
            );
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "home" sorts before "openai": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        // gpt-5 means nothing to a llama.cpp server: the picker opens.
        assert_eq!(effects, vec![Effect::FetchModels(FetchWhy::Bootstrap)]);
        assert!(matches!(h.app.screen, Screen::Picker(_)));
        assert_eq!(
            h.store.load().unwrap().effective(None).model,
            None,
            "the foreign model id did not survive the switch"
        );
    }

    #[test]
    fn d_forgets_the_highlighted_instance() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("llama-cpp".into());
            settings.instance = Some("home".into());
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(h.app.screen, Screen::Login(_)));
        let effective = h.store.load().unwrap().effective(None);
        assert!(effective.instances.is_empty());
        assert_eq!(effective.instance, None);
        // The provider stays llama-cpp, whose keyless server always
        // constructs — on the default URL now, the way a flat profile
        // always could.
        assert!(matches!(
            h.app.backend,
            Some(Backend::LlamaCpp { ref url, .. }) if url == "http://localhost:8080"
        ));
    }

    #[test]
    fn a_second_openai_compatible_instance_lands_under_its_own_name() {
        let mut h = harness(None, None, Session::default());
        // The openai row: a custom base URL, then the key.
        descend_to_credential(&mut h, 4);
        for c in "http://proxy:1234/v1".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        for c in "sk-p".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-p".into(),
                    base_url: Some("http://proxy:1234/v1".into()),
                },
                instance: Some("openai".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-p".into(),
                    base_url: Some("http://proxy:1234/v1".into()),
                },
                instance: Some("openai".into()),
            },
            result: loaded("openai", vec![llama_model("gpt-5")]),
        });
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(
            matches!(&h.app.backend, Some(Backend::OpenAi { base_url: Some(url), .. }) if url == "http://proxy:1234/v1"),
            "the custom base URL serves: {:?}",
            h.app.backend
        );

        // A second login of the same provider suggests a fresh name.
        crate::commands::dispatch(&mut h.app, "/login");
        let name = descend_to_credential(&mut h, 4);
        assert_eq!(name, "openai-2", "the suggestion skips the taken name");
    }

    #[test]
    fn login_failure_renders_inline_and_persists_nothing() {
        let mut h = harness(None, None, Session::default());
        descend_to_credential(&mut h, 0);
        for c in "bad-key".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "bad-key".into(),
                },
                instance: Some("google".into()),
            },
            result: failed("google", "gemini api error (HTTP 403): denied"),
        });
        match &h.app.screen {
            Screen::Login(login) => assert_eq!(
                login.status,
                crate::screens::LoginStatus::Error("gemini api error (HTTP 403): denied".into())
            ),
            _ => panic!("a failed login must stay on the login screen"),
        }
        let stored = h.store.load().unwrap();
        assert!(
            stored
                .profiles
                .values()
                .all(|profile| profile.instances.is_empty())
        );
    }

    #[test]
    fn the_copilot_login_runs_a_flow_and_persists_its_credential() {
        let mut h = harness(None, None, Session::default());
        // Walk down to the GitHub Copilot row (the last), name the
        // instance, and enter: the effect starts the flow, not a
        // credential input.
        h.app.on_key(key(KeyCode::Enter));
        for _ in 0..crate::screens::PROVIDERS.len() - 1 {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.stage, crate::screens::LoginStage::Flow);
                assert_eq!(
                    login.pending.as_ref().map(|p| p.provider),
                    Some(LoginProvider::Copilot)
                );
            }
            _ => panic!("expected the login screen in its flow stage"),
        }

        // The flow's instruction and progress render on the screen.
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Instruct(
                rhema_provider::LoginInstruction {
                    text: "open the page".into(),
                    url: Some("https://github.com/login/device".into()),
                    code: Some("ABCD-1234".into()),
                },
            )));
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Waiting(
                "waiting".into(),
            )));
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.flow_waiting.as_deref(), Some("waiting"));
                assert!(login.flow_instruction.is_some());
            }
            _ => panic!("expected the login screen mid-flow"),
        }

        // Completion validates the credential through the fetch path.
        let effects = h.app.on_event(UiEvent::LoginUpdate(
            rhema_provider::LoginUpdate::Completed("gho_token".into()),
        ));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Copilot {
                    api_key: "gho_token".into(),
                },
                instance: Some("copilot".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Copilot {
                    api_key: "gho_token".into(),
                },
                instance: Some("copilot".into()),
            },
            result: loaded("copilot", models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.models.len(), 2),
            _ => panic!("expected the picker after a successful flow login"),
        }
        assert_eq!(
            h.store
                .load()
                .unwrap()
                .effective(None)
                .copilot_api_key()
                .as_deref(),
            Some("gho_token")
        );
        assert!(matches!(
            h.app.backend,
            Some(Backend::Copilot { ref api_key }) if api_key == "gho_token"
        ));
    }

    #[test]
    fn a_failed_flow_renders_its_error_and_persists_nothing() {
        let mut h = harness(None, None, Session::default());
        for _ in 0..crate::screens::PROVIDERS.len() - 1 {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Failed(
                "authorization was denied on GitHub".into(),
            )));
        match &h.app.screen {
            Screen::Login(login) => assert_eq!(
                login.status,
                crate::screens::LoginStatus::Error("authorization was denied on GitHub".into())
            ),
            _ => panic!("expected the login screen"),
        }
        let stored = h.store.load().unwrap();
        assert!(
            stored
                .profiles
                .values()
                .all(|profile| profile.instances.is_empty())
        );
        // Esc returns to the instance list for a retry.
        h.app.on_key(key(KeyCode::Esc));
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.stage, crate::screens::LoginStage::Instances)
            }
            _ => panic!("expected the login screen's instance list"),
        }
    }

    #[test]
    fn choosing_a_model_persists_it_and_builds_the_agent() {
        let mut h = harness(Some("k"), None, Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(effects.is_empty());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-a")
        );
    }

    #[test]
    fn ctrl_p_rebuild_preserves_the_session() {
        let session = Session {
            history: vec![Message::text(Role::User, "earlier work")],
            driver_state: serde_json::Value::Null,
            cost_usd: 0.0,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
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
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-b")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("model switched to google/m-b".into()))
        );
    }

    #[test]
    fn esc_when_idle_asks_and_y_quits() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
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
    fn the_context_footprint_estimates_a_loaded_history() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // No reported usage and no history: zero, as before.
        assert_eq!(h.app.context_used(), 0);

        // A resumed history has already spent context: eight characters
        // estimate to two tokens. The agent owns the live history.
        h.app
            .agent
            .as_mut()
            .unwrap()
            .session_mut()
            .history
            .push(Message::text(Role::User, "12345678"));
        assert_eq!(h.app.context_used(), 2);

        // A reported turn wins over the estimate.
        h.app.usage = Some(Usage {
            input_tokens: 500,
            output_tokens: 3,
            cached_input_tokens: 0,
        });
        assert_eq!(h.app.context_used(), 500);
    }

    #[test]
    fn the_status_tracks_thinking_versus_running() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.running = true;
        h.app.begin_turn(None);

        // The step begins: thinking.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
        assert!(!h.app.processing);

        // A tool call arrives: running it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.processing);

        // The result lands: still running, until the next step or the end.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "on main".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(h.app.processing);

        // The turn ends: idle, no phase.
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
        });
        assert!(!h.app.processing);
    }

    #[test]
    fn paste_lands_in_the_input_with_newlines() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_paste("a\r\nb");
        assert_eq!(h.app.chat.input, "a\nb");
    }

    #[test]
    fn the_wheel_scrolls_the_same_step_as_the_page_keys() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_mouse_scroll(true);
        assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
        h.app.on_key(key(KeyCode::PageUp));
        assert_eq!(
            h.app.chat.scroll,
            2 * crate::screens::ChatState::SCROLL_STEP
        );
        h.app.on_mouse_scroll(false);
        assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
    }

    #[test]
    fn esc_when_running_cancels_the_turn_and_flags_the_channel() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        h.app.turn_cancel = Some(cancel);
        h.app.running = true;
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.decline_all, "upcoming approvals decline");
        assert!(!h.app.should_quit, "esc is not quit mid-turn");
        assert!(*cancelled.borrow(), "the turn task sees the flag");

        // The task's event closes the turn out.
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
        });
        assert!(!h.app.running);
        assert!(h.app.turn_cancel.is_none());
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cancelled")
        ));
    }

    #[test]
    fn esc_with_a_pending_modal_cancels_and_releases_the_blocked_call() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (hook, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || hook.approve(&test_call()));
        h.app.begin_turn(Some(answers));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        h.app.turn_cancel = Some(cancel);
        h.app.running = true;

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.pending_approval.is_some());
        h.app.on_key(key(KeyCode::Esc));
        assert!(!handle.join().unwrap(), "the parked call was declined");
        assert!(
            *cancelled.borrow(),
            "the turn was cancelled, not just declined"
        );
        assert!(h.app.pending_approval.is_none());
    }

    #[test]
    fn the_cancelled_event_hands_the_agent_back_with_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.running = true;
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
        });
        assert!(!h.app.running);
        assert!(h.app.agent.is_some());
        assert!(h.app.turn_cancel.is_none());
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cancelled")
        ));
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
            result: Err("stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit".into()),
        });
        assert!(!h.app.running);
        assert_eq!(
            h.app.error.as_deref(),
            Some(
                "stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit"
            )
        );
    }

    #[test]
    fn chat_submit_guards() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
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
    fn reasoning_previews_and_vanishes_when_output_starts() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("hmm".into())));
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta(" deeper".into())));
        assert_eq!(h.app.thinking, "hmm deeper");

        // Real output clears the buffer: the block vanishes.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta("the answer".into())));
        assert!(h.app.thinking.is_empty());

        // So does a tool call mid-turn.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("again".into())));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.thinking.is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit_from_any_screen() {
        let mut h = harness(None, None, Session::default());
        h.app.on_key(ctrl('c'));
        assert!(h.app.should_quit);

        let mut h = harness(None, None, Session::default());
        h.app.on_key(ctrl('d'));
        assert!(h.app.should_quit);
    }

    #[test]
    fn login_success_writes_into_the_selected_tab_profile() {
        let mut h = harness(None, None, Session::default());
        // Give the config a second profile so the login has a work tab.
        h.app
            .config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());
        // Re-open the login so its registry picks up the new profile.
        crate::commands::dispatch(&mut h.app, "/login");
        // Tab to the work profile on the login screen (Right cycles tabs),
        // then add an openrouter instance there.
        h.app.on_key(key(KeyCode::Right));
        match &h.app.screen {
            Screen::Login(login) => assert_eq!(login.profile, "work"),
            _ => panic!("expected the login screen"),
        }
        descend_to_credential(&mut h, 2);
        for c in "work-key".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenRouter {
                    api_key: "work-key".into(),
                },
                instance: Some("openrouter".into()),
            },
            result: loaded("google", models()),
        });
        // The session now runs on work, and the key landed in its section.
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        let stored = h.store.load().unwrap();
        let work = stored.effective(Some("work"));
        assert_eq!(work.instance.as_deref(), Some("openrouter"));
        assert_eq!(
            work.instances["openrouter"].api_key.as_deref(),
            Some("work-key")
        );
        assert_eq!(work.openrouter_api_key().as_deref(), Some("work-key"));
    }

    #[test]
    fn slash_profile_with_a_name_switches_the_backend() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.profiles.insert(
            "work".to_owned(),
            rhema_config::Profile {
                model: Some("work-model".into()),
                ..keyed_profile("openrouter", "work-key")
            },
        );
        crate::commands::dispatch(&mut h.app, "/profile work");
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(h.app.screen, Screen::Chat), "stays in the chat");
        match &h.app.backend {
            Some(Backend::OpenRouter { api_key }) => assert_eq!(api_key, "work-key"),
            other => panic!("expected the openrouter backend, got {other:?}"),
        }
        // The profiles' sections are untouched, but the explicit choice
        // is recorded for this workspace right away.
        let stored = h.store.load().unwrap();
        assert!(stored.profiles.contains_key("personal"));
        assert_eq!(
            stored.last_profile_in(&h.app.options.workspace).as_deref(),
            Some("work")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("switched to profile work".into()))
        );
    }

    #[test]
    fn startup_without_a_flag_starts_on_a_ready_profile() {
        let work_ready = rhema_config::Profile {
            model: Some("work-model".into()),
            ..keyed_profile("openrouter", "work-key")
        };
        // personal is empty, work is logged in: the session starts on
        // work instead of the login screen.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..Default::default()
        };
        assert_eq!(startup_profile(&config, Path::new("/nowhere")), "work");
        // personal can serve turns: the default stands.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed_profile("google", "k")),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..Default::default()
        };
        assert_eq!(startup_profile(&config, Path::new("/nowhere")), "personal");
        // Nothing can: the default stands and the login screen opens on
        // it.
        assert_eq!(
            startup_profile(&Config::default(), Path::new("/nowhere")),
            "personal"
        );
        // A key without a provider name is not usable either: nothing
        // defaults to google, so the logged-out-looking profile loses to
        // the ready one.
        let mut keyed = rhema_config::Profile::default();
        store_key(&mut keyed, "google", "k");
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed),
                ("work".to_owned(), work_ready),
            ]),
            ..Default::default()
        };
        assert_eq!(startup_profile(&config, Path::new("/nowhere")), "work");
    }

    #[test]
    fn a_workspace_reopens_on_the_profile_it_last_used() {
        let ready = |key: &str| keyed_profile("google", key);
        let mut config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), ready("k")),
                ("work".to_owned(), ready("w")),
            ]),
            ..Default::default()
        };
        // Both profiles can serve turns, and proj last ran on work: the
        // workspace memory wins over the personal default.
        config.note_profile_in(Path::new("/proj"), "work");
        assert_eq!(startup_profile(&config, Path::new("/proj")), "work");
        // Another directory has no memory: the default stands.
        assert_eq!(startup_profile(&config, Path::new("/other")), "personal");
        // A memory naming a profile that can no longer serve turns falls
        // through to the default — never the login screen by surprise.
        config
            .profiles
            .insert("empty".to_owned(), rhema_config::Profile::default());
        config.note_profile_in(Path::new("/proj"), "empty");
        assert_eq!(startup_profile(&config, Path::new("/proj")), "personal");
    }

    #[test]
    fn slash_thinking_persists_the_level_and_rebuilds_the_agent() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/thinking high");
        assert_eq!(
            h.app.options.thinking,
            rhema_core::request::ThinkingEffort::High
        );
        let stored = h.store.load().unwrap();
        let profile = h.app.session_profile.clone().unwrap_or_default();
        assert_eq!(
            stored.effective(Some(&profile)).thinking.as_deref(),
            Some("high")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("thinking effort set to high".into()))
        );

        // An unknown level is declined in-band, and default clears the
        // stored field.
        crate::commands::dispatch(&mut h.app, "/thinking maximal");
        assert!(h.app.error.is_none());
        crate::commands::dispatch(&mut h.app, "/thinking default");
        assert_eq!(
            h.app.options.thinking,
            rhema_core::request::ThinkingEffort::Default
        );
        assert_eq!(
            h.store.load().unwrap().effective(Some(&profile)).thinking,
            None
        );
    }

    #[test]
    fn a_turn_records_its_profile_for_the_workspace() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let workspace = h.app.options.workspace.clone();
        h.app.session_profile = Some("work".to_owned());
        h.app.begin_turn(None);
        assert_eq!(
            h.app.config.last_profile_in(&workspace).as_deref(),
            Some("work"),
            "the workspace remembers the profile"
        );
        // What the next launch reads is the saved file, not live state.
        assert_eq!(
            h.store
                .load()
                .unwrap()
                .last_profile_in(&workspace)
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn a_logged_in_profile_opens_the_chat_without_the_login_screen() {
        // The run_tui glue: resolve the no-flag profile, build the
        // backend from its section, and hand both to App::new. personal
        // is empty, work is ready with a model.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                (
                    "work".to_owned(),
                    rhema_config::Profile {
                        model: Some("work-model".into()),
                        ..keyed_profile("openrouter", "work-key")
                    },
                ),
            ]),
            ..Default::default()
        };
        let profile = startup_profile(&config, Path::new("/nowhere"));
        let backend = Backend::from_profile(&config.effective(Some(&profile)));
        let app = App::new(AppSetup {
            store: Arc::new(MemoryStore::new(config.clone())),
            config,
            backend,
            session: Session::default(),
            options: RunOptions {
                theme: crate::args::ThemeKind::Dark,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some(profile),
                workspace: PathBuf::from("."),
                max_steps: 4,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        assert!(matches!(app.screen, Screen::Chat), "no login screen");
        assert_eq!(app.session_profile.as_deref(), Some("work"));
        // The session reads work's section, so the footer shows its model.
        assert_eq!(app.effective().model.as_deref(), Some("work-model"));
    }

    #[test]
    fn export_skips_a_session_with_nothing_to_resume() {
        // A fresh session saves nothing: there is no conversation.
        let h = harness(None, None, Session::default());
        assert_eq!(h.app.export_session(), None);

        // A session of notices alone (a switch, a decline) saves nothing
        // either: the history, not the transcript, decides.
        let mut h = harness(None, None, Session::default());
        h.app.push_notice("switched to profile work");
        assert_eq!(h.app.export_session(), None);

        // One prompt in: the live session exports, with no origin file.
        let mut h = harness(Some("k"), None, Session::default());
        h.app
            .session
            .history
            .push(Message::text(Role::User, "fix it"));
        let (exported, origin) = h.app.export_session().expect("a spoken session exports");
        assert_eq!(exported.history.len(), 1);
        assert_eq!(origin, None);
    }

    #[test]
    fn slash_resume_opens_a_picker_and_picking_resumes() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // Two saved sessions for this workspace, newest first.
        let base = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(&h.app.options.workspace).unwrap();
        let older =
            crate::sessions::save_session(base.path(), &workspace, &session_of("the older prompt"))
                .unwrap()
                .unwrap();
        let newer =
            crate::sessions::save_session(base.path(), &workspace, &session_of("the newer prompt"))
                .unwrap()
                .unwrap();
        // Speak one line first: resuming replaces it.
        h.app.transcript.push_user("current talk");

        // The picker over the temp base; the dispatch path resolves its
        // own base from the environment, which a test cannot own.
        h.app.open_resume_from(base.path());
        match &h.app.screen {
            Screen::Resume(resume) => {
                assert_eq!(resume.entries.len(), 2);
                assert_eq!(resume.entries[0].path, newer, "newest first");
                assert_eq!(
                    resume.entries[0].snippet.as_deref(),
                    Some("the newer prompt")
                );
            }
            _other => panic!("expected the resume picker"),
        }

        // Down moves to the older session; enter loads it.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(
            h.app
                .session
                .history
                .first()
                .and_then(|m| match m.content.first() {
                    Some(rhema_core::Content::Text(text)) => Some(text.as_str()),
                    _ => None,
                }),
            Some("the older prompt"),
            "the older session is the live one"
        );
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User("the older prompt".into()))
        );
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.starts_with("resumed ")
        ));
        // The session is now bound to the file it came from: exiting
        // updates it in place instead of listing it twice.
        assert_eq!(h.app.resumed_from.as_deref(), Some(older.as_path()));
        let (exported, origin) = h.app.export_session().expect("a spoken session exports");
        assert_eq!(origin.as_deref(), Some(older.as_path()));
        assert_eq!(exported.history.len(), 1);
        // The file the picker named is what got loaded.
        assert!(older.is_file() && newer.is_file());
    }

    #[test]
    fn slash_resume_with_nothing_saved_is_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let base = tempfile::tempdir().unwrap();
        h.app.open_resume_from(base.path());
        assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("no saved sessions")
        ));
    }

    fn session_of(prompt: &str) -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        session
    }

    #[test]
    fn slash_new_starts_a_fresh_session() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .session
            .history
            .push(Message::text(Role::User, "talk"));
        h.app.transcript.push_user("talk");
        h.app.resumed_from = Some(PathBuf::from("/tmp/old.json"));

        crate::commands::dispatch(&mut h.app, "/new");

        // A fresh session: no history to save, no resume binding, the
        // transcript down to the notice.
        assert_eq!(h.app.export_session(), None);
        assert_eq!(h.app.resumed_from, None);
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cleared")
        ));
    }

    #[test]
    fn slash_skills_lists_what_discovery_finds() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let global = tempfile::tempdir().unwrap();
        let folder = global.path().join("commit-message");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\ndescription: apply when writing a commit\n---\nbody",
        )
        .unwrap();

        h.app
            .show_skills(Some(global.path()), Path::new("/nowhere"));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Markdown(table))
                if table.contains("name | trigger")
                    && table.contains("commit-message | apply when writing a commit")
        ));

        // No skills anywhere: the notice names the fix instead.
        let empty = tempfile::tempdir().unwrap();
        h.app.show_skills(Some(empty.path()), Path::new("/nowhere"));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("no skills")
        ));
    }

    #[test]
    fn slash_system_shows_the_prompt_in_a_fenced_block() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());

        crate::commands::dispatch(&mut h.app, "/system");

        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Markdown(text))
                if text.starts_with("```markdown\n") && text.ends_with("```")
        ));
        // An argument is declined like any no-argument command.
        crate::commands::dispatch(&mut h.app, "/system now");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("unknown")
        ));
    }

    #[test]
    fn model_choice_lands_in_the_session_profile() {
        // Started with no --profile (so personal), then switched to work:
        // the pick must follow the session, not the startup flag.
        let mut h = harness(Some("k"), None, Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "work-key"));
        crate::commands::dispatch(&mut h.app, "/profile work");
        crate::commands::dispatch(&mut h.app, "/model");
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: loaded("google", models()),
        });
        // Enter picks the first row (the picker sorts by id): "m-a".
        h.app.on_key(key(KeyCode::Enter));
        // The pick persists into work's section; personal's stays empty.
        let stored = h.store.load().unwrap();
        assert_eq!(stored.effective(Some("work")).model.as_deref(), Some("m-a"));
        assert_eq!(stored.effective(Some("personal")).model, None);
        // The rebuilt session reads the same section back.
        assert_eq!(h.app.effective().model.as_deref(), Some("m-a"));
    }

    #[test]
    fn slash_profile_without_credentials_still_switches() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.profiles.insert(
            "work".to_owned(),
            rhema_config::Profile {
                provider: Some("openrouter".into()),
                ..rhema_config::Profile::default()
            },
        );
        crate::commands::dispatch(&mut h.app, "/profile work");
        // The identity moves even without a key...
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("no API key")
        ));
        // ...but the old provider goes with the old profile: no backend,
        // no agent, no model list.
        assert!(h.app.backend.is_none());
        assert!(h.app.provider.is_none());
        assert!(h.app.agent.is_none());
        assert_eq!(h.app.effective().model, None, "reads work's section");

        // A prompt on a keyless profile is declined in-band and the text
        // stays for later.
        h.app.chat.input = "hello".to_owned();
        h.app.chat.cursor = 5;
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(h.app.chat.input == "hello", "the prompt is kept");
        assert!(!h.app.running, "no fake turn starts");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("/login")
        ));

        // /model is declined too: there is no backend to fetch from.
        crate::commands::dispatch(&mut h.app, "/model");
        assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");

        // And the way back rebuilds on the kept session.
        crate::commands::dispatch(&mut h.app, "/profile personal");
        assert!(h.app.backend.is_some());
        assert!(h.app.agent.is_some());
        assert_eq!(h.app.session_profile.as_deref(), Some("personal"));
    }

    #[test]
    fn slash_profile_alone_opens_the_picker() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/profile");
        match &h.app.screen {
            Screen::Profile(profile) => {
                assert_eq!(profile.entries.len(), 1);
                assert_eq!(profile.entries[0].name, "personal");
            }
            _ => panic!("expected the profile picker"),
        }
    }

    #[test]
    fn slash_model_opens_the_picker_and_esc_returns_to_the_chat() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/model");
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.purpose, PickerPurpose::FromChat);
            }
            _ => panic!("expected the picker"),
        }
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn slash_login_opens_the_login_and_esc_returns_without_quitting() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        match &h.app.screen {
            Screen::Login(login) => {
                assert_eq!(login.purpose, LoginPurpose::FromChat);
            }
            _ => panic!("expected the login screen"),
        }
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(!h.app.should_quit, "esc goes back, not out");
    }

    #[test]
    fn slash_login_success_chains_into_a_picker_that_knows_the_way_back() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "k".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.purpose, PickerPurpose::FromChat);
            }
            _ => panic!("expected the picker"),
        }
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn esc_while_entering_a_command_clears_it_without_quit_confirm() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        for c in "/clear".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.chat.input.is_empty());
        assert!(!h.app.confirm_quit, "esc cancels the command, not the app");
    }

    #[test]
    fn slash_model_during_a_turn_declines_with_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.running = true;
        crate::commands::dispatch(&mut h.app, "/model");
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("waits for it to finish")
        ));
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
            cost_usd: 0.0,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        crate::commands::dispatch(&mut h.app, "/clear");
        assert!(h.app.agent.unwrap().session().history.is_empty());
        assert!(
            h.app.transcript.lines().len() == 1,
            "only the notice remains"
        );
    }

    #[test]
    fn slash_exit_quits_and_unknown_commands_get_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/nope");
        assert!(!h.app.should_quit);
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("unknown command /nope")
        ));
        crate::commands::dispatch(&mut h.app, "/exit");
        assert!(h.app.should_quit);
    }
}
