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
    AgentError, Content, Message, ModelOutput, ModelProvider, Session, StopReason, ToolCall, Usage,
};
use rhema_provider::{Allowance, ModelCard};

use crate::approval::ApprovalAnswerer;
use crate::events::{FetchWhy, OwnedEvent, UiEvent};
use crate::screens::{
    AddInstanceDialog, ChatAction, ChatState, PickerAction, PickerPurpose, PickerScreen,
    PickerStatus, ProfileAction, ProfileScreen, SettingsAction, SettingsPurpose, SettingsScreen,
    SettingsSection, SkillRow,
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
    /// The keychain fresh logins place keys in and removed logins
    /// clean up after; `None` keeps every key in the file (tests, or a
    /// runner that opted out).
    pub secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
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
    /// Z.ai's GLM Coding Plan, by API key (their Anthropic-compatible
    /// endpoint): an instance may name a base URL (a proxy for the route);
    /// `None` speaks to the Coding Plan route itself.
    Zai {
        /// The resolved key (`env` first, stored second).
        api_key: String,
        /// The instance's base URL, when it stored one.
        base_url: Option<String>,
    },
    /// GitHub Copilot, driven by the device flow's long-lived GitHub
    /// OAuth token (the adapter exchanges it for short-lived tokens).
    Copilot {
        /// The resolved GitHub token (`env` first, stored second).
        api_key: String,
    },
}

impl Backend {
    /// The provider implementation for this backend: the raw adapter
    /// under the shared retry, so a transient failure costs a pause
    /// instead of the turn (see `rhema_provider::Retrying`).
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        let raw: Arc<dyn ModelProvider> = match self {
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
            Backend::Zai { api_key, base_url } => Arc::new(Self::zai_provider(api_key, base_url)),
            Backend::Copilot { api_key } => Arc::new(rhema_provider_copilot::CopilotProvider::new(
                api_key.clone(),
            )),
        };
        Arc::new(rhema_provider::Retrying::new(raw))
    }

    /// The Coding Plan provider: the stored base URL when an instance
    /// overrode it, the Coding Plan route otherwise — always with bearer
    /// auth, the scheme that route documents.
    fn zai_provider(
        api_key: &str,
        base_url: &Option<String>,
    ) -> rhema_provider_anthropic::AnthropicProvider {
        let base = base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(rhema_provider_anthropic::ZAI_BASE_URL);
        rhema_provider_anthropic::AnthropicProvider::new(api_key)
            .with_base_url(base)
            .with_bearer_auth()
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
            Backend::Zai { .. } => "zai",
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
    pub fn from_profile(
        profile: &rhema_config::Profile,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Option<Backend> {
        Self::from_profile_or_help(profile, secrets).ok()
    }

    /// [`Backend::from_profile`] with the failure explained: the headless
    /// runner has no login screen to fall back to, so its error names the
    /// fix instead of leaving it to a screen that does not exist. The TUI
    /// flattens it back to `None` and opens the login.
    pub fn from_profile_or_help(
        profile: &rhema_config::Profile,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Result<Backend, String> {
        let active = profile
            .provider_name()
            .filter(|name| rhema_config::provider_info(name).is_some())
            .ok_or_else(|| NO_PROVIDER_HELP.to_owned())?;
        let missing = || {
            let help = rhema_config::provider_info(&active)
                .map(|info| info.missing_key_help)
                .unwrap_or("missing API key");
            match secrets {
                // A store that does not answer reads as "not in the
                // store": say so, or a locked keychain reads as a key
                // the user never stored.
                Some(store) if !store.reachable() => format!(
                    "{help} — the system keychain is locked or unreachable, \
                     so the stored keys were unreadable"
                ),
                _ => help.to_owned(),
            }
        };
        if let Some((_, instance)) = profile.instance_for_provider(&active) {
            return Self::from_instance(profile, instance, secrets).ok_or_else(missing);
        }
        Self::named(&active, profile, secrets).ok_or_else(missing)
    }

    /// Builds one backend per logged-in instance of the profile — the
    /// active one first, then the rest in name order — so the picker
    /// spans every llama.cpp server, OpenAI-compatible endpoint, and
    /// Copilot sub the profile holds. A profile without instances falls
    /// back to the environment alone (llama.cpp only when active: a
    /// URL is a server address, not a login).
    pub fn candidates_from_profile(
        profile: &rhema_config::Profile,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Vec<Candidate> {
        let active = profile.provider_name();
        let active_entry = active
            .as_deref()
            .and_then(|kind| profile.instance_for_provider(kind))
            .map(|(name, _)| name.to_owned());
        let mut candidates = Vec::new();
        if let Some(kind) = active.as_deref() {
            if let Some((name, instance)) = profile.instance_for_provider(kind) {
                if let Some(backend) = Self::from_instance(profile, instance, secrets) {
                    candidates.push(Candidate {
                        instance: Some(name.to_owned()),
                        backend,
                    });
                }
            } else if let Some(backend) = Self::named(kind, profile, secrets) {
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
            if let Some(backend) = Self::from_instance(profile, instance, secrets) {
                candidates.push(Candidate {
                    instance: Some(name.to_owned()),
                    backend,
                });
            }
        }
        // The login rows only: a llama.cpp server is an address to
        // point at, not a subscription, so the environment alone never
        // nominates one.
        for info in rhema_config::PROVIDERS.iter().filter(|info| info.login) {
            let name = info.name;
            if Some(name) == active.as_deref() || profile.instance_for_provider(name).is_some() {
                continue;
            }
            if let Some(backend) = Self::named(name, profile, secrets) {
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
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Option<Backend> {
        match instance.provider.as_str() {
            "llama-cpp" => Some(Backend::LlamaCpp {
                url: profile
                    .instance_url(instance)
                    .unwrap_or_else(|| profile.llama_cpp_url()),
                api_key: profile.instance_api_key(instance, secrets),
            }),
            "openrouter" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::OpenRouter { api_key }),
            "anthropic" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Anthropic { api_key }),
            "openai" => {
                profile
                    .instance_api_key(instance, secrets)
                    .map(|api_key| Backend::OpenAi {
                        api_key,
                        base_url: profile.instance_url(instance),
                    })
            }
            "deepseek" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::DeepSeek { api_key }),
            "zai" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Zai {
                    api_key,
                    base_url: profile.instance_url(instance),
                }),
            "copilot" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Copilot { api_key }),
            "google" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Google { api_key }),
            // A kind no row answers to builds nothing — an unknown
            // spelling never reads as google.
            _ => None,
        }
    }

    /// The backend for one provider's config name from the environment
    /// alone, `None` when the environment carries no usable credential
    /// for it. Profiles with a stored instance resolve through
    /// [`Backend::from_profile`] instead.
    fn named(
        provider: &str,
        profile: &rhema_config::Profile,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Option<Backend> {
        match provider {
            "google" => profile
                .api_key("google", secrets)
                .map(|api_key| Backend::Google { api_key }),
            "llama-cpp" => Some(Backend::LlamaCpp {
                url: profile.llama_cpp_url(),
                api_key: profile.api_key("llama-cpp", secrets),
            }),
            "openrouter" => profile
                .api_key("openrouter", secrets)
                .map(|api_key| Backend::OpenRouter { api_key }),
            "anthropic" => profile
                .api_key("anthropic", secrets)
                .map(|api_key| Backend::Anthropic { api_key }),
            "openai" => profile
                .api_key("openai", secrets)
                .map(|api_key| Backend::OpenAi {
                    api_key,
                    base_url: None,
                }),
            "deepseek" => profile
                .api_key("deepseek", secrets)
                .map(|api_key| Backend::DeepSeek { api_key }),
            "zai" => profile.api_key("zai", secrets).map(|api_key| Backend::Zai {
                api_key,
                base_url: None,
            }),
            "copilot" => profile
                .api_key("copilot", secrets)
                .map(|api_key| Backend::Copilot { api_key }),
            _ => None,
        }
    }

    /// The backend's subscription allowance, when its provider reports
    /// one: prepaid credit on OpenRouter and DeepSeek, the seat's
    /// premium requests on Copilot, the Coding Plan's 5-hour and weekly
    /// windows on Z.ai. `Ok(None)` for the providers with no such
    /// endpoint — the footer just never shows their badge.
    pub async fn subscription_usage(&self) -> Result<Option<Allowance>, String> {
        match self {
            Backend::OpenRouter { api_key } => {
                rhema_provider_openrouter::OpenRouterProvider::new(api_key.clone())
                    .allowance()
                    .await
                    .map(Some)
                    .map_err(|err| err.to_string())
            }
            Backend::Copilot { api_key } => {
                rhema_provider_copilot::CopilotProvider::new(api_key.clone())
                    .allowance()
                    .await
                    .map(Some)
                    .map_err(|err| err.to_string())
            }
            Backend::DeepSeek { api_key } => {
                rhema_provider_openai_compatible::OpenAiCompatibleProvider::new(api_key.clone())
                    .with_base_url("https://api.deepseek.com/v1")
                    .balance()
                    .await
                    .map(Some)
                    .map_err(|err| err.to_string())
            }
            Backend::Zai { api_key, base_url } => Self::zai_provider(api_key, base_url)
                .allowance()
                .await
                .map_err(|err| err.to_string()),
            // No allowance endpoint to ask: Google keys, local servers,
            // Anthropic, the generic OpenAI-compatible family.
            Backend::Google { .. }
            | Backend::LlamaCpp { .. }
            | Backend::Anthropic { .. }
            | Backend::OpenAi { .. } => Ok(None),
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
            Backend::Zai { api_key, base_url } => Self::zai_provider(api_key, base_url)
                .list_models()
                .await
                .map_err(|err| err.to_string()),
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

/// The fix for a config that names no usable provider: the startup gate
/// and the headless error share it.
pub(crate) const NO_PROVIDER_HELP: &str = "no provider configured: set RHEMA_PROVIDER, pick one in the UI, or \
     store one in a profile";

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
    /// Which color palette paints the screen.
    pub colors: crate::palette::Palette,
    /// The palette's appearance: its dark tokens or its light ones.
    pub theme: crate::palette::ThemeKind,
    /// The M3 You seed, `0xRRGGBB`; `None` is Material's baseline
    /// purple. Every palette but M3 You ignores it.
    pub m3_source: Option<u32>,
    /// The custom palette's token overrides; every palette but Custom
    /// ignores it.
    pub custom: Option<rhema_config::CustomPalette>,
    /// Which icon flavor the footer draws with.
    pub icons: crate::icons::Flavor,
    /// The config profile this session runs under.
    pub profile: Option<String>,
    /// Workspace root for tools.
    pub workspace: PathBuf,
    /// Max steps per turn.
    pub max_steps: u32,
    /// Bound long sessions with the transcript's request view.
    pub transcript: bool,
    /// How much reasoning the model should do.
    pub thinking: rhema_core::request::ThinkingEffort,
    /// The base prompt from the user's SYSTEM.md, when their file
    /// exists; `None` uses the built-in default.
    pub base_system: Option<String>,
    /// Approve all gated tool calls without asking.
    /// Checkpoint the session after every turn.
    pub save: Option<PathBuf>,
}

impl RunOptions {
    /// The per-startup inputs the generated palettes read.
    pub(crate) fn palette_inputs(&self) -> crate::palette::PaletteInputs<'_> {
        crate::palette::PaletteInputs {
            m3_source: self.m3_source,
            custom: self.custom.as_ref(),
        }
    }
}

/// The current screen. The settings screen is boxed: its live providers
/// page plus its snapshots outweigh the hot keypath's taste for a small
/// `Screen` (the same reason the login it absorbed was boxed once).
pub enum Screen {
    /// Choose a model.
    Picker(PickerScreen),
    /// Switch the profile the session runs under.
    Profile(ProfileScreen),
    /// Pick a saved session of this workspace to continue.
    Resume(crate::screens::ResumeScreen),
    /// The settings screen: a sidebar of sections; the Providers page
    /// carries the login flow, the appearance page the theme.
    Settings(Box<SettingsScreen>),
    /// The conversation.
    Chat,
}

/// What a left click on the settings screen landed on. The renderer
/// registers the rects every frame; the app matches the position of the
/// click against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsHit {
    /// A sidebar row: jump to this section.
    Sidebar(SettingsSection),
    /// A page row (an instance, the add row, an appearance toggle):
    /// select it and act.
    PageRow(usize),
    /// A provider row of the add-instance dialog.
    DialogProvider(usize),
}

/// A side effect the runner must perform (network or task spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Fetch the model list for `why`.
    FetchModels(FetchWhy),
    /// Fetch the active backend's subscription allowance, if its
    /// provider reports one.
    FetchUsage,
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
    /// The keychain logins place keys in; `None` keeps them in the file.
    pub secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
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
    /// The active backend's subscription allowance, when one was
    /// fetched: the backend's config name rides along, so a report of a
    /// backend the session has since left never shows. A failed fetch
    /// clears the badge — a stale figure reads as truth.
    pub subscription: Option<(&'static str, Option<Allowance>)>,
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
    /// The turn lifecycle: what the keyboard, the renderer, and the
    /// status tracker need while the agent is out with a task.
    pub turn: TurnState,
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
    /// The quit confirmation overlay is up.
    pub confirm_quit: bool,
    /// The cancel handle of the running login flow; `None` while idle.
    /// Dropping it ends the flow task — the watch receiver errors out —
    /// exactly what Esc and a completed flow both want.
    pub login_cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// The file this session was resumed from, if any: the exit save
    /// updates it in place instead of growing a second entry in /resume.
    pub resumed_from: Option<PathBuf>,
    /// Set by the runner to end the event loop.
    pub should_quit: bool,
    /// The resolved design tokens every render pass reads.
    pub theme: crate::palette::Theme,
    /// The resolved footer glyphs every render pass reads.
    pub icons: &'static crate::icons::Icons,
    /// The chat input state.
    pub chat: ChatState,
    /// The clickable rects of the settings screen, re-registered by
    /// every frame's render and matched against click positions.
    pub settings_hits: Vec<(ratatui::layout::Rect, SettingsHit)>,
    /// The transcript render's row cache: wrapped rows per block,
    /// invalided by width, theme, and the transcript's watermark.
    pub(crate) transcript_cache: crate::ui::WrapCache,
}

/// The lifecycle state of the turn the UI thread handed to a task. The
/// agent comes back in a `TurnFinished` or `TurnCancelled` event; this is
/// what the keyboard, the renderer, and the status tracker need until
/// then. Between turns everything rests at its [`TurnState::idle`] default.
pub struct TurnState {
    /// Whether a turn is in flight.
    pub running: bool,
    /// When the turn started, for the elapsed-time indicator.
    pub started_at: Option<std::time::Instant>,
    /// The cancel flag of the running turn; the task watches the
    /// receiver. The runner creates the pair at submit.
    pub cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// The tool call currently awaiting the user's answer.
    pub pending_approval: Option<ToolCall>,
    /// Every future approval request is declined without a modal (the
    /// turn is being cancelled; nothing more may ask).
    pub decline_all: bool,
    /// The approval answer channel; `None` while no turn runs, making
    /// any late answer a no-op.
    answers: Option<ApprovalAnswerer>,
    /// A command is executing while the turn runs; `false` while the
    /// model itself is streaming — thinking.
    pub processing: bool,
    /// The running turn's reasoning trace, tail only: the status strip
    /// previews its last rows, and it clears the moment real output
    /// starts. Never part of the transcript or the session history.
    pub thinking: String,
    /// The running command's output, tail only: the status strip
    /// previews its completed lines while the command runs, and the
    /// whole text lands with the tool result. Never part of the
    /// transcript or the session history.
    pub tool_output: String,
    /// When the running command's first output chunk landed: the
    /// preview stays hidden until the command has earned it — a second
    /// of runtime or a second line.
    pub tool_output_at: Option<std::time::Instant>,
}

impl TurnState {
    /// The state between turns.
    fn idle() -> Self {
        Self {
            running: false,
            started_at: None,
            cancel: None,
            pending_approval: None,
            decline_all: false,
            answers: None,
            processing: false,
            thinking: String::new(),
            tool_output: String::new(),
            tool_output_at: None,
        }
    }

    /// A turn was accepted: the clock starts in the thinking phase, the
    /// approval answer channel is wired, and the previous turn's residue
    /// (a stale modal, a preview buffer) is gone.
    fn running(
        cancel: tokio::sync::watch::Sender<bool>,
        answers: Option<ApprovalAnswerer>,
    ) -> Self {
        Self {
            running: true,
            started_at: Some(std::time::Instant::now()),
            cancel: Some(cancel),
            answers,
            ..Self::idle()
        }
    }

    /// The turn is over, finished or cancelled: one reset for both, so
    /// they cannot drift.
    fn finished(&mut self) {
        self.running = false;
        self.started_at = None;
        self.cancel = None;
        self.processing = false;
        self.thinking.clear();
        self.decline_all = false;
        self.pending_approval = None;
        self.answers = None;
    }

    /// Answers the pending approval and clears the modal; without a
    /// listening turn task, a no-op.
    fn answer(&mut self, approved: bool) {
        if let Some(answers) = &self.answers {
            answers.answer(approved);
        }
        self.pending_approval = None;
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

    /// Appends one output chunk of the running command to the tool
    /// preview buffer, tail-capped like the reasoning buffer; the first
    /// chunk starts the reveal clock.
    fn push_tool_output(&mut self, chunk: &str) {
        const TAIL_CAP: usize = 2048;
        if self.tool_output_at.is_none() {
            self.tool_output_at = Some(std::time::Instant::now());
        }
        self.tool_output.push_str(chunk);
        if self.tool_output.len() > TAIL_CAP {
            let mut cut = self.tool_output.len() - TAIL_CAP;
            while !self.tool_output.is_char_boundary(cut) {
                cut += 1;
            }
            self.tool_output.drain(..cut);
        }
    }

    /// The running command's preview goes: its result landed, or the
    /// next call started.
    fn clear_tool_output(&mut self) {
        self.tool_output.clear();
        self.tool_output_at = None;
    }
}

/// One keypress routed to the active screen.
enum Routed {
    Picker(PickerAction),
    Profile(ProfileAction),
    Resume(crate::screens::ResumeAction),
    Settings(SettingsAction),
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
                let agent_config =
                    wired_config(provider.clone(), &setup.options, effective.model.clone());
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
            // No usable backend: the settings' providers page is the
            // front door now. Esc there quits — there is no chat behind
            // it to return to. The failed resolution explains itself on
            // the page: a locked keychain must not read as a bare
            // login screen.
            (None, _) => {
                let mut settings = Self::build_settings_screen(
                    &setup.config,
                    &setup.options,
                    setup.options.profile.as_deref(),
                    SettingsPurpose::Initial,
                );
                settings.startup_reason =
                    Backend::from_profile_or_help(&effective, setup.secrets.as_deref()).err();
                Screen::Settings(Box::new(settings))
            }
        };
        Ok(Self {
            secrets: setup.secrets,
            // Read before `options` moves into the struct.
            theme: setup
                .options
                .colors
                .theme(setup.options.theme, setup.options.palette_inputs()),
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
                    .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned()),
            ),
            options: setup.options,
            home: std::env::var("HOME").ok().map(PathBuf::from),
            subscription: None,
            session,
            screen,
            models: Vec::new(),
            agent,
            turn: TurnState::idle(),
            transcript,
            usage: None,
            session_cost: 0.0,
            error: None,
            confirm_quit: false,
            login_cancel: None,
            resumed_from: None,
            should_quit: false,
            chat: ChatState::new(),
            settings_hits: Vec::new(),
            transcript_cache: crate::ui::WrapCache::new(),
        })
    }

    /// Effects the runner must perform once, before the first draw.
    pub fn initial_effects(&self) -> Vec<Effect> {
        if self.backend.is_some() {
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
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
            self.secret_store(),
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

    /// Opens the settings screen on `section`. `/settings` and `/login`
    /// both land on Providers — the login flow lives there now. The
    /// Providers page edits the session's profile live; the other pages
    /// read snapshots of this moment. Openable mid-turn: only the
    /// instance actions wait for an idle session.
    pub fn open_settings_at(&mut self, section: SettingsSection, purpose: SettingsPurpose) {
        let mut settings = Self::build_settings_screen(
            &self.config,
            &self.options,
            self.session_profile.as_deref(),
            purpose,
        );
        settings.section = section;
        settings.allowance = self.subscription.clone();
        settings.last_turn = self.usage;
        settings.session_cost = self.session_cost;
        self.screen = Screen::Settings(Box::new(settings));
    }

    /// Assembles the settings screen: the live Providers page over the
    /// session's profile, plus the other pages' snapshots.
    fn build_settings_screen(
        config: &Config,
        options: &RunOptions,
        session_profile: Option<&str>,
        purpose: SettingsPurpose,
    ) -> SettingsScreen {
        let effective = config.effective(session_profile);
        let workspace =
            std::fs::canonicalize(&options.workspace).unwrap_or_else(|_| options.workspace.clone());
        let global_skills = rhema_config::Config::skills_dir().ok();
        let skills = rhema_agent_tools::skills::discover(global_skills.as_deref(), &workspace)
            .into_iter()
            .map(|skill| SkillRow {
                name: skill.name,
                description: skill.description,
            })
            .collect();
        let mut settings = SettingsScreen::new(purpose);
        settings.colors = options.colors;
        settings.theme_dark = options.theme == crate::palette::ThemeKind::Dark;
        settings.icons_nerd = options.icons == crate::icons::Flavor::Nerd;
        settings.profile = session_profile
            .unwrap_or(rhema_config::DEFAULT_PROFILE)
            .to_owned();
        settings.stored = effective;
        settings.skills = skills;
        settings.skills_global_dir = global_skills.map(|dir| dir.to_string_lossy().into_owned());
        settings.skills_workspace_dir = workspace
            .join(".rhema")
            .join("skills")
            .to_string_lossy()
            .into_owned();
        settings.config_path = rhema_config::Config::path()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        settings.workspace = workspace.to_string_lossy().into_owned();
        settings.profile_count = config.profiles.len();
        settings
    }

    /// Flips the appearance and persists the choice: the swap is live
    /// (the tokens change this frame), and the config records it for the
    /// next startup. The open settings page follows, so it keeps telling
    /// the truth.
    fn toggle_theme(&mut self) {
        let dark = self.options.theme == crate::palette::ThemeKind::Dark;
        let kind = if dark {
            crate::palette::ThemeKind::Light
        } else {
            crate::palette::ThemeKind::Dark
        };
        self.options.theme = kind;
        self.theme = self
            .options
            .colors
            .theme(kind, self.options.palette_inputs());
        self.config.theme = Some(if dark { "light" } else { "dark" }.to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.theme_dark = !dark;
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Cycles to the next palette and persists the choice, live like the
    /// theme toggle. The open settings page follows.
    fn cycle_palette(&mut self) {
        let all = crate::palette::Palette::ALL;
        let current = all
            .iter()
            .position(|palette| *palette == self.options.colors)
            .unwrap_or(0);
        let next = all[(current + 1) % all.len()];
        self.options.colors = next;
        self.theme = next.theme(self.options.theme, self.options.palette_inputs());
        self.config.colors = Some(next.stored_name().to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.colors = next;
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Flips the footer's icon flavor and persists the choice, live like
    /// the theme toggle.
    fn toggle_icons(&mut self) {
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
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
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
    pub fn switch_profile(&mut self, name: &str) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /profile waits for it to finish");
            return Vec::new();
        }
        if self.session_profile.as_deref() == Some(name) {
            self.transcript
                .push_notice(&format!("already on profile {name}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(name));
        self.options.thinking = effective.thinking_effort();
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let Some(backend) = Backend::from_profile(&effective, self.secret_store()) else {
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
            return Vec::new();
        };
        let agent_config = wired_config(backend.provider(), &self.options, effective.model.clone());
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
                // A new profile is a new plan: the allowance badge
                // fetches now, not at the first turn's end.
                vec![Effect::FetchUsage]
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
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
        let skills = rhema_agent_tools::skills::discover(global, workspace);
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
        if self.turn.running {
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
        if self.turn.running {
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
    /// pasting multi-line text must not fire submits. The add-instance
    /// dialog's credential line takes its own, single-line shape.
    pub fn on_paste(&mut self, text: &str) {
        match &mut self.screen {
            Screen::Chat => self.chat.paste(text),
            Screen::Settings(settings) => {
                if let Some(dialog) = &mut settings.dialog {
                    dialog.paste(text);
                }
            }
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

    /// A left click landed. The settings menu answers clicks — sidebar
    /// rows switch sections, page rows select-and-act, the add dialog's
    /// provider rows pick — everywhere else the keyboard rules. The hit
    /// rects are re-registered by every frame's render, so a click can
    /// only land on what was last drawn.
    pub fn on_click(&mut self, x: u16, y: u16) -> Vec<Effect> {
        if !matches!(self.screen, Screen::Settings(_)) {
            return Vec::new();
        }
        let hit = self
            .settings_hits
            .iter()
            .find(|(area, _)| area.contains(ratatui::layout::Position::new(x, y)))
            .map(|(_, hit)| *hit);
        let Some(hit) = hit else {
            return Vec::new();
        };
        let action = match hit {
            SettingsHit::Sidebar(section) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                if settings.dialog.is_some() {
                    // The dialog is modal: clicks behind it do nothing.
                    return Vec::new();
                }
                settings.section = section;
                settings.row = 0;
                return Vec::new();
            }
            SettingsHit::PageRow(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                if settings.dialog.is_some() {
                    return Vec::new();
                }
                settings.row = index;
                settings.enter_action()
            }
            SettingsHit::DialogProvider(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                let Some(dialog) = &mut settings.dialog else {
                    return Vec::new();
                };
                dialog.select_provider_row(index);
                return Vec::new();
            }
        };
        self.perform_settings(action)
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
            Screen::Picker(picker) => Routed::Picker(picker.handle_key(key)),
            Screen::Profile(profile) => Routed::Profile(profile.handle_key(key)),
            Screen::Resume(resume) => Routed::Resume(resume.handle_key(key)),
            Screen::Settings(settings) => Routed::Settings(settings.handle_key(key)),
            Screen::Chat => {
                let profiles = self.palette_profiles();
                Routed::Chat(self.chat.handle_key(key, &self.models, &profiles))
            }
        };
        match routed {
            Routed::Settings(action) => self.perform_settings(action),
            Routed::Picker(PickerAction::Choose {
                provider,
                instance,
                id,
            }) => self.choose_model(provider, instance.as_deref(), &id),
            Routed::Picker(PickerAction::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Routed::Picker(PickerAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Profile(ProfileAction::Switch(name)) => self.switch_profile(&name),
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
            Routed::Picker(PickerAction::None)
            | Routed::Profile(ProfileAction::None)
            | Routed::Resume(crate::screens::ResumeAction::None) => Vec::new(),
            Routed::Chat(chat_action) => self.on_chat_key(key, chat_action),
        }
    }

    /// Performs one settings action, from a keypress or a click.
    fn perform_settings(&mut self, action: SettingsAction) -> Vec<Effect> {
        match action {
            SettingsAction::None => Vec::new(),
            SettingsAction::Back => {
                // With a session behind the settings, Esc means "back";
                // at the startup gate it quits.
                if matches!(
                    &self.screen,
                    Screen::Settings(settings) if settings.purpose == SettingsPurpose::FromChat
                ) {
                    self.screen = Screen::Chat;
                } else {
                    self.should_quit = true;
                }
                Vec::new()
            }
            SettingsAction::ActivateInstance => self.activate_instance(),
            SettingsAction::RemoveInstance => self.remove_instance(),
            SettingsAction::OpenDialog => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
                Vec::new()
            }
            SettingsAction::Validate => self.validate_login(),
            SettingsAction::StartFlow => vec![Effect::StartLoginFlow],
            SettingsAction::CloseDialog => {
                // The dialog is already gone from the screen; dropping
                // the handle ends the flow task (the watch receiver
                // errors out) — what Esc and a completed flow both want.
                self.login_cancel = None;
                Vec::new()
            }
            SettingsAction::CyclePalette => {
                self.cycle_palette();
                Vec::new()
            }
            SettingsAction::ToggleTheme => {
                self.toggle_theme();
                Vec::new()
            }
            SettingsAction::ToggleIcons => {
                self.toggle_icons();
                Vec::new()
            }
        }
    }

    /// Chat keys that are the app's, not the input box's.
    fn on_chat_key(&mut self, key: KeyEvent, action: ChatAction) -> Vec<Effect> {
        // Esc during a running turn cancels the turn, from anywhere —
        // approval modal included: the pending answer is released, the
        // rest declined, and the task drops the turn at its next await.
        // The y/N modal keys keep answering single calls.
        if self.turn.running && key.code == KeyCode::Esc {
            self.cancel_turn();
            return Vec::new();
        }
        // The approval modal takes every key while it is up.
        if self.turn.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.turn.answer(true),
                KeyCode::Char('n' | 'N') => self.turn.answer(false),
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
            if key.code == KeyCode::Char('p') && !self.turn.running {
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
            if !self.turn.running {
                self.confirm_quit = true;
            }
            return Vec::new();
        }
        match action {
            ChatAction::Submit(text) => {
                if self.turn.running || text.trim().is_empty() {
                    // The input is kept, so typing during a turn is not
                    // lost.
                    Vec::new()
                } else if text.trim().starts_with('/') {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.attachments.clear();
                    // Commands read plain text: a paste embedded in the
                    // line expands for them, as it would for the model.
                    crate::commands::dispatch(self, rhema_core::paste::expand(&text).trim())
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
                    self.turn.running = true;
                    self.turn.started_at = Some(std::time::Instant::now());
                    self.error = None;
                    self.turn.decline_all = false;
                    vec![Effect::StartTurn(text)]
                }
            }
            ChatAction::None => Vec::new(),
        }
    }

    /// The add-instance dialog validated a new instance's input: fetch
    /// the models; the fetch is the validation (a wrong key or a dead
    /// server both surface as the fetch's error).
    fn validate_login(&mut self) -> Vec<Effect> {
        let pending = match &self.screen {
            Screen::Settings(settings) => settings
                .dialog
                .as_ref()
                .and_then(|dialog| dialog.pending.clone()),
            _ => None,
        };
        let Some(pending) = pending else {
            return Vec::new();
        };
        let input = match &self.screen {
            Screen::Settings(settings) => settings
                .dialog
                .as_ref()
                .map(|dialog| dialog.input.trim().to_owned())
                .unwrap_or_default(),
            _ => String::new(),
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

    /// Enter on a logged-in instance: it becomes the profile's active
    /// one, and the agent rebuilds carrying the conversation over — a
    /// picker when no model is stored yet, the chat otherwise.
    fn activate_instance(&mut self) -> Vec<Effect> {
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
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(&tab));
        self.options.thinking = effective.thinking_effort();
        let Some(backend) = Backend::from_profile(&effective, self.secret_store()) else {
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
                    // The session now rides the instance it activated:
                    // that plan's numbers are the badge's to fetch.
                    vec![Effect::FetchUsage]
                } else {
                    self.screen = Screen::Picker(PickerScreen {
                        status: PickerStatus::Loading,
                        ..PickerScreen::new(self.models.clone(), PickerPurpose::Initial)
                    });
                    vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
                }
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
            }
        }
    }

    /// `d` on a logged-in instance: forget it. When it was the active
    /// one, the backend re-resolves from what remains — another instance
    /// of the provider, or none.
    fn remove_instance(&mut self) -> Vec<Effect> {
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
        if let Some(id) = removed.and_then(|old| old.secret_ref) {
            if let Some(store) = self.secret_store() {
                let _ = store.delete(&id);
            }
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        let mut effects = Vec::new();
        if self.session_profile.as_deref() == Some(tab.as_str()) {
            let effective = self.config.effective(Some(&tab));
            match Backend::from_profile(&effective, self.secret_store()) {
                Some(backend) => {
                    self.backend = Some(backend.clone());
                    self.provider = Some(backend.provider());
                    // What remains is a different plan: refresh the
                    // badge against it.
                    effects.push(Effect::FetchUsage);
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

    /// A step of the running login flow: render it on the add-instance
    /// dialog, and on completion validate the credential it produced
    /// through the same fetch path a typed key takes.
    fn on_login_update(&mut self, update: rhema_provider::LoginUpdate) -> Vec<Effect> {
        let Screen::Settings(settings) = &mut self.screen else {
            // The flow was left behind (Esc raced a completed update):
            // drop the credential with the screen it belonged to.
            self.login_cancel = None;
            return Vec::new();
        };
        let Some(dialog) = &mut settings.dialog else {
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
            UiEvent::ModelsLoaded { why, result } => self.on_models_loaded(why, result),
            UiEvent::LoginUpdate(update) => self.on_login_update(update),
            UiEvent::ApprovalRequested { call } => {
                if self.turn.decline_all {
                    // Esc declined the rest of the turn: answer without a
                    // modal so the blocked turn task keeps going.
                    self.turn.answer(false);
                } else {
                    self.turn.pending_approval = Some(call);
                }
                Vec::new()
            }
            UiEvent::UsageLoaded { provider, result } => {
                // Err or a switched-away backend both degrade to a
                // smaller badge, never to a stale figure.
                self.subscription = result.ok().map(|allowance| (provider, allowance));
                Vec::new()
            }
            UiEvent::TurnFinished { agent, result } => {
                self.end_turn(agent, result);
                // The turn just moved the account's counters; the badge
                // is only as good as its last fetch.
                vec![Effect::FetchUsage]
            }
            UiEvent::TurnCancelled { agent } => {
                self.cancel_finished(agent);
                vec![Effect::FetchUsage]
            }
        }
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    fn on_models_loaded(
        &mut self,
        why: FetchWhy,
        result: Vec<crate::events::CatalogFetch>,
    ) -> Vec<Effect> {
        match why {
            FetchWhy::Login { backend, instance } => {
                let (from_chat, tab) = match &self.screen {
                    Screen::Settings(settings) => (
                        settings.purpose == SettingsPurpose::FromChat,
                        settings.profile.clone(),
                    ),
                    // The dialog was left behind (Esc raced a completed
                    // fetch): the login has nowhere to land.
                    _ => return Vec::new(),
                };
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
                            Backend::OpenAi { api_key, base_url }
                            | Backend::Zai { api_key, base_url } => {
                                (Some(api_key.clone()), base_url.clone())
                            }
                            Backend::Google { api_key }
                            | Backend::OpenRouter { api_key }
                            | Backend::Anthropic { api_key }
                            | Backend::DeepSeek { api_key }
                            | Backend::Copilot { api_key } => (Some(api_key.clone()), None),
                        };
                        // The key rides the keychain when the store takes
                        // it — the instance keeps only the entry's ref —
                        // and the file when it does not.
                        let secrets = self.secrets.as_deref();
                        self.config.set_for_profile(Some(&tab), |settings| {
                            // A re-login replaces the instance wholesale;
                            // its old keychain entry must not linger.
                            if let Some(old) = settings.instances.get(&instance) {
                                if let Some(id) = old.secret_ref.as_deref() {
                                    if let Some(store) = secrets {
                                        let _ = store.delete(id);
                                    }
                                }
                            }
                            settings.provider = Some(provider.to_owned());
                            settings.instance = Some(instance.clone());
                            settings.upsert_instance(
                                &instance,
                                rhema_config::ProviderInstance {
                                    provider: provider.to_owned(),
                                    api_key: None,
                                    secret_ref: None,
                                    base_url,
                                },
                            );
                            if let Some(key) = api_key.as_deref() {
                                settings.place_key(&instance, key, secrets);
                            }
                        });
                        if let Err(err) = self.store.save(&self.config) {
                            if let Screen::Settings(settings) = &mut self.screen {
                                if let Some(dialog) = &mut settings.dialog {
                                    dialog.status = crate::screens::DialogStatus::Error(format!(
                                        "cannot save config: {err}"
                                    ));
                                }
                            }
                            return Vec::new();
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
                            return self.choose_model(provider, Some(&instance), &id);
                        }
                        let purpose = if from_chat {
                            PickerPurpose::FromChat
                        } else {
                            PickerPurpose::Initial
                        };
                        self.screen =
                            Screen::Picker(PickerScreen::new(self.models.clone(), purpose));
                        // A login installs the backend it validated: that
                        // plan is the session's numbers now.
                        vec![Effect::FetchUsage]
                    }
                    Some(Err(message)) => {
                        if let Screen::Settings(settings) = &mut self.screen {
                            if let Some(dialog) = &mut settings.dialog {
                                dialog.on_validated(&Err(message));
                            }
                        }
                        Vec::new()
                    }
                    // The dispatch always spawns one backend per login;
                    // an empty result cannot arrive.
                    None => {
                        if let Screen::Settings(settings) = &mut self.screen {
                            if let Some(dialog) = &mut settings.dialog {
                                dialog.on_validated(&Err("no provider answered".to_owned()));
                            }
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
                        // The single-model failover applies at startup
                        // too: a server with one offering has nothing
                        // to pick. Refresh (ctrl-p) always shows the
                        // list instead.
                        if picker.purpose == PickerPurpose::Initial && picker.models.len() == 1 {
                            // Nothing follows in this arm: the chat
                            // screen choose_model installed stands.
                            let entry = picker.models[0].clone();
                            return self.choose_model(
                                entry.provider,
                                entry.instance.as_deref(),
                                &entry.model.id,
                            );
                        }
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
    /// agent, carrying the current session over. The old agent is only
    /// dropped once the new one built, so a rebuild failure cannot strand
    /// the chat.
    fn choose_model(
        &mut self,
        provider: &'static str,
        instance: Option<&str>,
        id: &str,
    ) -> Vec<Effect> {
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
            return Vec::new();
        }
        let backend = match Backend::from_profile(&self.effective(), self.secret_store()) {
            Some(backend) => backend,
            None => {
                self.error = Some(format!(
                    "no usable credentials for {provider}; /login first"
                ));
                return Vec::new();
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
                if from_chat || matches!(&self.screen, Screen::Chat) {
                    self.transcript
                        .push_notice(&format!("model switched to {provider}/{id}"));
                }
                self.agent = Some(agent);
                self.screen = Screen::Chat;
                // A new backend counts a new plan: the allowance badge
                // fetches now, not at the first turn's end.
                vec![Effect::FetchUsage]
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
            }
        }
    }

    /// Builds an agent over `session` with the current model and flags.
    fn build_agent(&self, session: Session) -> Result<CodingAgent, CodingAgentError> {
        let provider = self
            .provider
            .clone()
            .ok_or(CodingAgentError::Agent(AgentError::MissingProvider))?;
        let agent_config = wired_config(
            provider.clone(),
            &self.options,
            self.effective().model.clone(),
        );
        CodingAgent::from_session(provider, agent_config, session)
    }

    /// The runner accepted a turn: a fresh lifecycle state with the
    /// cancel flag and the approval answer channel wired, starting in
    /// the thinking phase.
    pub fn begin_turn(
        &mut self,
        cancel: tokio::sync::watch::Sender<bool>,
        answers: Option<ApprovalAnswerer>,
    ) {
        self.turn = TurnState::running(cancel, answers);
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
    /// it — the thinking block vanishes once the answer starts. A
    /// running command's lines fill the tool preview; its result or the
    /// next call clears that.
    fn track_phase(&mut self, owned: &OwnedEvent) {
        match owned {
            OwnedEvent::StepStart { .. } | OwnedEvent::TextDelta(_) => {
                self.turn.processing = false;
                self.turn.thinking.clear();
            }
            OwnedEvent::ThinkingDelta(delta) => self.turn.push_thinking(delta),
            // A tool call means a command runs. Its arguments often never
            // stream as deltas, so the label stays phase-only; the last
            // command's preview goes with it.
            OwnedEvent::ToolCallStart { .. } => {
                self.turn.processing = true;
                self.turn.thinking.clear();
                self.turn.clear_tool_output();
            }
            OwnedEvent::ToolOutputDelta { chunk, .. } => self.turn.push_tool_output(chunk),
            OwnedEvent::ToolResult { .. } => self.turn.clear_tool_output(),
            _ => {}
        }
    }

    /// Cancels the running turn from the keyboard. The approval bridge
    /// blocks its thread on the answer, so a pending modal is released
    /// first; upcoming approvals are declined; then the flag flips and
    /// the turn task drops the future at its current await. The agent
    /// comes back in a `TurnCancelled`.
    fn cancel_turn(&mut self) {
        self.turn.answer(false);
        self.turn.decline_all = true;
        if let Some(cancel) = &self.turn.cancel {
            let _ = cancel.send(true);
        }
    }

    /// The turn task was cancelled from the keyboard: take the agent
    /// back, drop the turn's state, and mark the cutoff in the transcript.
    fn cancel_finished(&mut self, agent: Box<CodingAgent>) {
        self.agent = Some(*agent);
        self.turn.finished();
        self.transcript.push_notice("turn cancelled");
    }

    /// The turn task finished: take the agent back, record the usage or
    /// the error, and drop any stale modal (the turn cannot finish while
    /// an approval is unanswered, so a leftover modal means the answer
    /// channel is gone).
    fn end_turn(&mut self, agent: Box<CodingAgent>, result: Result<ModelOutput, String>) {
        self.agent = Some(*agent);
        self.turn.finished();
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
                // Silence is a symptom, not an answer: the usual cause is
                // a reasoning-only response — a thinking model spending
                // its whole output budget before any text started — and
                // the turn must not read as completed when nothing was
                // said.
                let said_something = output.message.content.iter().any(|content| {
                    matches!(content, Content::Text(text) if !text.is_empty())
                        || matches!(content, Content::ToolCall(_))
                });
                if !said_something {
                    self.push_notice(&format!(
                        "the model returned no content (stopped: {})",
                        Self::stop_words(&output.stop_reason)
                    ));
                }
            }
            Err(message) => self.error = Some(message),
        }
    }

    /// The stop reason as the few words a notice names it by.
    fn stop_words(reason: &StopReason) -> String {
        match reason {
            StopReason::EndTurn => "end turn".to_owned(),
            StopReason::ToolUse => "tool use".to_owned(),
            StopReason::MaxTokens => "max tokens".to_owned(),
            StopReason::StopSequence => "stop sequence".to_owned(),
            StopReason::Other(other) => other.clone(),
        }
    }

    /// The settings that apply to this session's profile: field-wise, the
    /// named profile (or the one chosen at startup) over the top-level
    /// section. Computed on demand — it changes mid-session.
    pub fn effective(&self) -> rhema_config::Profile {
        self.config.effective(self.session_profile.as_deref())
    }

    /// The keychain this session resolves stored keys through, as the
    /// resolution tier under the environment; `None` reads literals only.
    pub fn secret_store(&self) -> Option<&dyn rhema_config::secrets::SecretStore> {
        self.secrets.as_deref()
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
            custom.unwrap_or_else(|| crate::palette::default_profile_color(&name, &self.theme));
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
    /// prompt plus its reply — the reply rides into the next request, so
    /// both count toward the window — otherwise an estimate over the
    /// loaded history, which already includes every message. A resumed
    /// session has spent context before its first turn here, and an
    /// empty one estimates to zero.
    pub fn context_used(&self) -> u64 {
        match &self.usage {
            Some(usage) => context_tokens(usage),
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
        transcript: options
            .transcript
            .then(rhema_transcript::TranscriptSettings::default),
        thinking: options.thinking,
        base_system: options.base_system.clone(),
        ..CodingAgentConfig::new(model.unwrap_or_default(), &options.workspace)
    }
}

/// The coding-agent config with discovered subagents wired in: when any
/// definition exists, the shared shell and the `subagent` tool attach;
/// otherwise the config comes back untouched.
fn wired_config(
    provider: Arc<dyn ModelProvider>,
    options: &RunOptions,
    model: Option<String>,
) -> CodingAgentConfig {
    rhema_subagents::wire(provider, agent_config(options, model))
}

/// The context a reported usage stands for: the whole prompt plus the
/// reply, the shape of the next request. The adapters normalize
/// `input_tokens` to the inclusive convention — the cached subset
/// included — so the prompt total describes the conversation, never
/// just the tokens a cache miss had to re-read.
pub(crate) fn context_tokens(usage: &Usage) -> u64 {
    u64::from(usage.input_tokens) + u64::from(usage.output_tokens)
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
pub(crate) fn startup_profile(
    config: &rhema_config::Config,
    workspace: &Path,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> String {
    let default = rhema_config::DEFAULT_PROFILE;
    let ready = |name: &str| {
        config
            .profiles
            .get(name)
            .map(|profile| Backend::from_profile(profile, secrets).is_some())
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
    use rhema_config::secrets::SecretStore as _;
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
        /// The keychain the app places logins in, when the test gave it
        /// one.
        secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
    }

    /// Stores `key` as `provider`'s instance on `profile` — the login
    /// flow's write shape.
    fn store_key(profile: &mut rhema_config::Profile, provider: &str, key: &str) {
        profile.upsert_instance(
            provider,
            rhema_config::ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some(key.to_owned()),
                secret_ref: None,
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
        harness_inner(api_key, model, session, None)
    }

    /// A harness whose app places logins in a volatile keychain.
    fn harness_with_secrets(
        api_key: Option<&str>,
        model: Option<&str>,
        session: Session,
    ) -> Harness {
        harness_inner(
            api_key,
            model,
            session,
            Some(Arc::new(rhema_config::secrets::VolatileSecretStore::new())),
        )
    }

    fn harness_inner(
        api_key: Option<&str>,
        model: Option<&str>,
        session: Session,
        secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
    ) -> Harness {
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
                    secret_ref: None,
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
            secrets: secrets
                .clone()
                .map(|store| store as Arc<dyn rhema_config::secrets::SecretStore>),
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: crate::palette::ThemeKind::Dark,
                m3_source: None,
                custom: None,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some("personal".into()),
                workspace: PathBuf::from("."),
                max_steps: 4,
                transcript: false,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        Harness {
            app,
            store,
            secrets,
        }
    }

    /// Drives the settings' add-instance dialog to the credential stage
    /// for a provider sitting at `row` on the provider list, accepting
    /// the suggested instance name — ready to type the credential. The
    /// settings must be open (or `/login` dispatchable) first.
    fn descend_to_credential(h: &mut Harness, row: usize) -> String {
        use crate::screens::DialogStage;
        if !matches!(h.app.screen, Screen::Settings(_)) {
            crate::commands::dispatch(&mut h.app, "/login");
        }
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        // Down to the add row of the instance list, then enter.
        for _ in 0..settings.instance_rows().len().saturating_sub(1) {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        for _ in 0..row {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        let dialog = settings.dialog.as_ref().expect("the add dialog");
        let name = dialog.input.clone();
        assert_eq!(dialog.stage, DialogStage::NameInstance);
        // Enter accepts the suggested name; copilot's flow starts
        // instead of asking for a key, which its own tests drive.
        h.app.on_key(key(KeyCode::Enter));
        name
    }

    #[test]
    fn a_paste_on_the_add_dialog_fills_the_credential() {
        let mut h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        // Descend into the credential stage first.
        descend_to_credential(&mut h, 0);
        h.app.on_paste("sk-123");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.dialog.as_ref().unwrap().input, "sk-123");
    }

    #[test]
    fn no_resolvable_key_opens_the_settings_gate() {
        let h = harness(None, None, Session::default());
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, crate::screens::SettingsSection::Providers);
        assert_eq!(settings.purpose, SettingsPurpose::Initial);
        assert!(h.app.initial_effects().is_empty());
    }

    /// A store that refuses everything: the locked-keychain shape the
    /// reachability probe exists to catch.
    struct LockedStore;

    impl rhema_config::secrets::SecretStore for LockedStore {
        fn reachable(&self) -> bool {
            false
        }

        fn get(&self, _id: &str) -> Result<Option<String>, rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }

        fn set(&self, _id: &str, _value: &str) -> Result<(), rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }

        fn delete(&self, _id: &str) -> Result<bool, rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }
    }

    #[test]
    fn the_startup_settings_say_why_they_opened() {
        let h = harness(None, None, Session::default());
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        let reason = settings
            .startup_reason
            .as_deref()
            .expect("the gate names why no backend resolved");
        assert!(reason.contains("GEMINI_API_KEY"), "{reason}");
    }

    #[test]
    fn settings_opened_from_the_chat_carry_no_reason() {
        let mut h = harness(Some("k"), None, Session::default());
        h.app.open_settings_at(
            crate::screens::SettingsSection::Providers,
            SettingsPurpose::FromChat,
        );
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.startup_reason, None);
    }

    #[test]
    fn a_locked_keychain_is_named_by_the_missing_key_help() {
        let profile = rhema_config::Profile {
            provider: Some("google".into()),
            ..rhema_config::Profile::default()
        };
        let err = Backend::from_profile_or_help(&profile, Some(&LockedStore)).unwrap_err();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
        assert!(err.contains("locked or unreachable"), "{err}");
    }

    #[test]
    fn the_plain_missing_key_help_names_no_keychain() {
        let profile = rhema_config::Profile {
            provider: Some("google".into()),
            ..rhema_config::Profile::default()
        };
        let err = Backend::from_profile_or_help(&profile, None).unwrap_err();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
        assert!(!err.contains("keychain"), "{err}");
    }

    #[test]
    fn a_key_without_a_stored_model_opens_a_loading_picker() {
        let mut h = harness(Some("k"), None, Session::default());
        assert!(matches!(h.app.screen, Screen::Picker(_)));
        assert_eq!(
            h.app.initial_effects(),
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
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
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        );
    }

    /// A landed allowance rides with the backend that fetched it; a
    /// failed fetch clears the badge rather than leaving a stale
    /// figure that reads as truth.
    #[test]
    fn the_allowance_lands_and_a_failed_fetch_clears_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::UsageLoaded {
            provider: "google",
            result: Ok(Some(rhema_provider::Allowance {
                amount: "$4.12".to_owned(),
                kind: String::new(),
            })),
        });
        let (provider, allowance) = h.app.subscription.as_ref().expect("allowance landed");
        assert_eq!(*provider, "google");
        assert_eq!(allowance.as_ref().expect("present").amount, "$4.12");

        h.app.on_event(UiEvent::UsageLoaded {
            provider: "google",
            result: Err("network error".to_owned()),
        });
        assert!(h.app.subscription.is_none());
    }

    /// The turn just moved the account's counters: finishing (or
    /// cancelling) one schedules the allowance refresh.
    #[test]
    fn a_finished_turn_refreshes_the_allowance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let agent = Box::new(h.app.agent.take().unwrap());
        let effects = h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        });
        assert_eq!(effects, vec![Effect::FetchUsage]);
    }

    /// A reasoning-only response — a content-less assistant message the
    /// provider ended itself — cannot pass for an answer: the transcript
    /// names what happened. A reply that said something raises nothing.
    #[test]
    fn an_empty_turn_surfaces_its_stop_reason() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message {
                    role: Role::Assistant,
                    content: vec![],
                },
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
            }),
        });
        assert!(h.app.transcript.lines().contains(&Line::Notice(
            "the model returned no content (stopped: max tokens)".into()
        )));

        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        });
        // The first turn's notice rightly persists; a real reply raised
        // no second one.
        assert_eq!(
            h.app
                .transcript
                .lines()
                .iter()
                .filter(|line| matches!(
                    line,
                    Line::Notice(text) if text.contains("no content")
                ))
                .count(),
            1
        );
    }

    /// A model switch lands a new backend: its allowance fetches at
    /// once, not at the first turn's end.
    #[test]
    fn a_model_switch_refreshes_the_allowance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let effects = h.app.choose_model("google", Some("google"), "m-a");
        assert_eq!(effects, vec![Effect::FetchUsage]);
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
        assert_eq!(effective.api_key("google", None).as_deref(), Some("secret"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::Google { ref api_key }) if api_key == "secret"
        ));
    }

    #[test]
    fn a_login_places_its_key_in_the_keychain_and_the_file_stays_clean() {
        let mut h = harness_with_secrets(None, None, Session::default());
        let secrets = Arc::clone(h.secrets.as_ref().unwrap());
        descend_to_credential(&mut h, 0);
        for c in "secret".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let _ = h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        let effective = h.store.load().unwrap().effective(None);
        let instance = &effective.instances["google"];
        assert_eq!(instance.api_key, None, "the file holds no key");
        let id = instance.secret_ref.as_deref().expect("the keychain ref");
        assert_eq!(secrets.get(id).unwrap().as_deref(), Some("secret"));
        // The session's own resolution reads the chain: the login
        // serves turns with no literal anywhere.
        assert_eq!(
            h.app
                .effective()
                .api_key("google", h.app.secret_store())
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn removing_an_instance_deletes_its_keychain_entry() {
        let mut h = harness_with_secrets(Some("g"), Some("m"), Session::default());
        let secrets = Arc::clone(h.secrets.as_ref().unwrap());
        // The profile's one instance logs in through the chain: the
        // ref is live, the literal is gone.
        secrets.set("ref-1", "g").unwrap();
        h.app.config.set_for_profile(Some("personal"), |p| {
            let instance = p.instances.get_mut("google").unwrap();
            instance.secret_ref = Some("ref-1".into());
            instance.api_key = None;
        });
        // /login opens the providers page over the session's profile;
        // the instance row is selected, `d` forgets it.
        for c in "/login".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        let _ = h.app.on_key(key(KeyCode::Char('d')));
        let effective = h.store.load().unwrap().effective(None);
        assert!(!effective.instances.contains_key("google"), "forgotten");
        assert_eq!(secrets.get("ref-1").unwrap(), None, "the entry went too");
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
    fn a_late_bootstrap_keeps_the_login_models() {
        // The startup fetch races a login: it was spawned first, but a
        // slow catalog can land it after the login's answer. Its slices
        // replace themselves; the login's slice is not its business.
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
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
            [Effect::FetchModels(FetchWhy::Login { .. })]
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
        assert_eq!(h.app.models.len(), 4);

        // The late startup answer covers google only.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        let providers: Vec<&str> = h.app.models.iter().map(|m| m.provider).collect();
        assert_eq!(
            providers,
            ["google", "google", "openrouter", "openrouter"],
            "the login's slice survives the late bootstrap"
        );
    }

    #[test]
    fn llama_login_validates_by_server_url_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // The llama.cpp row is second on the add flow's provider list.
        let name = descend_to_credential(&mut h, 1);
        assert_eq!(name, "llama-cpp");
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
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
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
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
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
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
        assert_eq!(
            effective.api_key("anthropic", None).as_deref(),
            Some("sk-ant-01")
        );
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
            Screen::Settings(s) => assert!(matches!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error(_)
            )),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.app.backend.is_none());
        assert!(h.store.load().unwrap().effective(None).instances.is_empty());
    }

    #[test]
    fn zai_login_validates_by_key_with_the_default_route() {
        let mut h = harness(None, None, Session::default());
        // Z.ai is the seventh provider row.
        descend_to_credential(&mut h, 6);
        // The base URL prompt comes first: an empty one means the Coding
        // Plan route itself.
        h.app.on_key(key(KeyCode::Enter));
        for c in "zkey-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Zai {
                    api_key: "zkey-01".into(),
                    base_url: None,
                },
                instance: Some("zai".into()),
            })]
        );
        // A failed validation stays on the login screen and persists
        // nothing — same contract as the other keyed providers.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Zai {
                    api_key: "zkey-01".into(),
                    base_url: None,
                },
                instance: Some("zai".into()),
            },
            result: failed("zai", "anthropic api error (HTTP 401): denied"),
        });
        match &h.app.screen {
            Screen::Settings(s) => assert!(matches!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error(_)
            )),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.app.backend.is_none());
        assert!(h.store.load().unwrap().effective(None).instances.is_empty());
    }

    #[test]
    fn a_zai_profile_builds_its_own_backend_from_a_stored_route() {
        // A stored instance with a base URL override: the backend points
        // the adapter at that route instead of the Coding Plan's.
        let mut profile = keyed_profile("zai", "k-z");
        profile.instance = Some("zai".into());
        profile.upsert_instance(
            "proxy",
            rhema_config::ProviderInstance {
                provider: "zai".into(),
                api_key: Some("k-p".into()),
                secret_ref: None,
                base_url: Some("https://proxy.example/api/anthropic".into()),
            },
        );
        let backend =
            Backend::from_profile(&profile, None).expect("a keyed zai profile serves turns");
        assert_eq!(backend.config_name(), "zai");
        assert!(
            matches!(
                backend,
                Backend::Zai { ref api_key, base_url: None }
                    if api_key == "k-z"
            ),
            "the active zai instance serves: {backend:?}"
        );
        // Activating the proxy instance moves both the key and the route.
        profile.instance = Some("proxy".into());
        assert!(
            matches!(
                Backend::from_profile(&profile, None),
                Some(Backend::Zai { ref api_key, base_url: Some(ref url) })
                    if api_key == "k-p" && url == "https://proxy.example/api/anthropic"
            ),
            "the proxy instance's route serves"
        );
        // No key, no backend — the kind is not served by another's key.
        let mut cross = rhema_config::Profile {
            provider: Some("zai".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut cross, "anthropic", "k-a");
        assert_eq!(
            Backend::from_profile(&cross, None),
            None,
            "a zai profile is not served by an anthropic key"
        );
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
        let backend = Backend::from_profile(&keyed("anthropic", Some("k-a")), None)
            .expect("a keyed anthropic profile serves turns");
        assert_eq!(backend.config_name(), "anthropic");
        assert!(
            matches!(backend, Backend::Anthropic { ref api_key } if api_key == "k-a"),
            "the anthropic profile builds the anthropic backend: {backend:?}"
        );
        let backend = Backend::from_profile(&keyed("openai", Some("k-o")), None)
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
            Backend::from_profile(&cross, None),
            None,
            "an anthropic profile is not served by an openai key"
        );
        assert_eq!(
            Backend::from_profile(
                &rhema_config::Profile {
                    provider: Some("openai".into()),
                    ..rhema_config::Profile::default()
                },
                None,
            ),
            None
        );
        // No provider named, and a spelling no provider answers to, both
        // leave the backend unresolved: the login flow runs, nothing
        // silently serves google.
        assert_eq!(
            Backend::from_profile(&rhema_config::Profile::default(), None),
            None,
            "a provider-less profile opens the login flow"
        );
        let mut nonsense = rhema_config::Profile {
            provider: Some("nonsense".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut nonsense, "google", "k");
        assert_eq!(
            Backend::from_profile(&nonsense, None),
            None,
            "an unknown spelling never falls back to google"
        );
    }

    #[test]
    fn candidates_span_every_provider_the_profile_holds_keys_for() {
        let mut profile = keyed_profile("openrouter", "o");
        store_key(&mut profile, "google", "g");
        let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
            .iter()
            .map(|candidate| candidate.backend.config_name())
            .collect();
        assert_eq!(names, ["openrouter", "google"], "the active provider leads");

        // llama.cpp joins only as the active provider: a server address
        // is a login, not a catalog to browse.
        profile.provider = Some("llama-cpp".into());
        let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
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
                        secret_ref: None,
                        base_url: Some("http://gpu:8080".into()),
                    },
                ),
                (
                    "home".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        secret_ref: None,
                        base_url: Some("http://home:8080".into()),
                    },
                ),
                (
                    "work sub".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "copilot".into(),
                        api_key: Some("gho-1".into()),
                        secret_ref: None,
                        base_url: None,
                    },
                ),
            ]),
            ..rhema_config::Profile::default()
        };
        let candidates = Backend::candidates_from_profile(&profile, None);
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
        assert_eq!(
            effects,
            vec![Effect::FetchUsage],
            "a named model rebuilds inline"
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
        assert_eq!(effects, vec![Effect::FetchUsage], "a pick rebuilds inline");
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
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
            settings.upsert_instance(
                "gpu box",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://gpu:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "gpu box" sorts before "home": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchUsage],
            "a stored model goes straight to chat"
        );
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
                    secret_ref: None,
                    base_url: None,
                },
            );
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "home" sorts before "openai": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        // gpt-5 means nothing to a llama.cpp server: the picker opens.
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        );
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
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(h.app.screen, Screen::Settings(_)));
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
            Screen::Settings(s) => assert_eq!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error("gemini api error (HTTP 403): denied".into())
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
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(login.stage, crate::screens::DialogStage::Flow);
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
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
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
                .api_key("copilot", None)
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
            Screen::Settings(s) => assert_eq!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error("authorization was denied on GitHub".into())
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
        // Esc closes the dialog back onto the Providers page for a retry.
        h.app.on_key(key(KeyCode::Esc));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_none());
    }

    #[test]
    fn choosing_a_model_persists_it_and_builds_the_agent() {
        let mut h = harness(Some("k"), None, Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::FetchUsage]);
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

        // A reported turn wins over the estimate: prompt plus reply,
        // the shape of the next request.
        h.app.usage = Some(Usage {
            input_tokens: 500,
            output_tokens: 3,
            cached_input_tokens: 0,
        });
        assert_eq!(h.app.context_used(), 503);
    }

    #[test]
    fn the_status_tracks_thinking_versus_running() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(cancel, None);

        // The step begins: thinking.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
        assert!(!h.app.turn.processing);

        // A tool call arrives: running it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.processing);

        // The result lands: still running, until the next step or the end.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "on main".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(h.app.turn.processing);

        // The turn ends: idle, no phase.
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
        });
        assert!(!h.app.turn.processing);
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
        h.app.turn.cancel = Some(cancel);
        h.app.turn.running = true;
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.turn.decline_all, "upcoming approvals decline");
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
        assert!(!h.app.turn.running);
        assert!(h.app.turn.cancel.is_none());
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
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(cancel, Some(answers));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        h.app.turn.cancel = Some(cancel);
        h.app.turn.running = true;

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.turn.pending_approval.is_some());
        h.app.on_key(key(KeyCode::Esc));
        assert!(!handle.join().unwrap(), "the parked call was declined");
        assert!(
            *cancelled.borrow(),
            "the turn was cancelled, not just declined"
        );
        assert!(h.app.turn.pending_approval.is_none());
    }

    #[test]
    fn the_cancelled_event_hands_the_agent_back_with_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
        });
        assert!(!h.app.turn.running);
        assert!(h.app.agent.is_some());
        assert!(h.app.turn.cancel.is_none());
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
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(cancel, Some(answers));

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.turn.pending_approval.is_some());
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
        h.app.turn.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage,
            }),
        });
        assert!(!h.app.turn.running);
        assert!(h.app.error.is_none());
        assert_eq!(h.app.usage, Some(usage));

        h.app.turn.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Err("stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit".into()),
        });
        assert!(!h.app.turn.running);
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
        assert!(!h.app.turn.running);
        // While a turn runs, typing works but Enter is inert.
        h.app.turn.running = true;
        for c in "hi".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(h.app.chat.input, "hi");
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(h.app.turn.running);
        // Idle again: Enter submits, shows the user line, and blocks
        // re-entry.
        h.app.turn.running = false;
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::StartTurn("hi".into())]
        );
        assert!(h.app.turn.running);
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User("hi".into()))
        );
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn a_submit_travels_embedded_and_lands_in_the_transcript() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let long = "l1\nl2\nl3\nl4\nl5";
        h.app.chat.paste(long);
        let embedded = rhema_core::paste::embed(&ChatState::MARK.to_string(), &[long.to_owned()]);
        // The turn starts on the session form, and the transcript line
        // carries it — the chip survives the send.
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::StartTurn(embedded.clone())]
        );
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User(embedded))
        );
    }

    /// A running command's output fills the preview buffer from its
    /// first chunk; the next call or the result takes it back.
    #[test]
    fn a_running_commands_output_previews_and_goes_with_its_result() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
            id: "c1".into(),
            chunk: "one\n".into(),
        }));
        assert_eq!(h.app.turn.tool_output, "one\n");
        assert!(
            h.app.turn.tool_output_at.is_some(),
            "the first chunk clocks in"
        );

        // The next command takes the last one's preview with it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c2".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.tool_output.is_empty());
        assert!(h.app.turn.tool_output_at.is_none());

        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
            id: "c2".into(),
            chunk: "two\n".into(),
        }));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c2".into(),
            content: "ok".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(h.app.turn.tool_output.is_empty());
        assert!(h.app.turn.tool_output_at.is_none());
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
        assert_eq!(h.app.turn.thinking, "hmm deeper");

        // Real output clears the buffer: the block vanishes.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta("the answer".into())));
        assert!(h.app.turn.thinking.is_empty());

        // So does a tool call mid-turn.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("again".into())));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.thinking.is_empty());
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
        // The session moves to the work profile, whose Providers page
        // the settings then edit, and an openrouter instance lands there.
        crate::commands::dispatch(&mut h.app, "/profile work");
        crate::commands::dispatch(&mut h.app, "/login");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.profile, "work");
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
        assert_eq!(
            work.api_key("openrouter", None).as_deref(),
            Some("work-key")
        );
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
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "work"
        );
        // personal can serve turns: the default stands.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed_profile("google", "k")),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "personal"
        );
        // Nothing can: the default stands and the login screen opens on
        // it.
        assert_eq!(
            startup_profile(&Config::default(), Path::new("/nowhere"), None),
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
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "work"
        );
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
        assert_eq!(startup_profile(&config, Path::new("/proj"), None), "work");
        // Another directory has no memory: the default stands.
        assert_eq!(
            startup_profile(&config, Path::new("/other"), None),
            "personal"
        );
        // A memory naming a profile that can no longer serve turns falls
        // through to the default — never the login screen by surprise.
        config
            .profiles
            .insert("empty".to_owned(), rhema_config::Profile::default());
        config.note_profile_in(Path::new("/proj"), "empty");
        assert_eq!(
            startup_profile(&config, Path::new("/proj"), None),
            "personal"
        );
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

        // An unknown level is declined in-band, and default stores its
        // name: the config never carries a bare null, which would read
        // as medium on the next load.
        crate::commands::dispatch(&mut h.app, "/thinking maximal");
        assert!(h.app.error.is_none());
        crate::commands::dispatch(&mut h.app, "/thinking default");
        assert_eq!(
            h.app.options.thinking,
            rhema_core::request::ThinkingEffort::Default
        );
        assert_eq!(
            h.store.load().unwrap().effective(Some(&profile)).thinking,
            Some("default".to_owned())
        );
    }

    #[test]
    fn a_turn_records_its_profile_for_the_workspace() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let workspace = h.app.options.workspace.clone();
        h.app.session_profile = Some("work".to_owned());
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(cancel, None);
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
        let profile = startup_profile(&config, Path::new("/nowhere"), None);
        let backend = Backend::from_profile(&config.effective(Some(&profile)), None);
        let app = App::new(AppSetup {
            store: Arc::new(MemoryStore::new(config.clone())),
            config,
            backend,
            session: Session::default(),
            secrets: None,
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: crate::palette::ThemeKind::Dark,
                m3_source: None,
                custom: None,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some(profile),
                workspace: PathBuf::from("."),
                max_steps: 4,
                transcript: false,
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
    fn slash_settings_opens_the_live_providers_page() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Providers);
        assert_eq!(settings.profile, "personal");
        assert_eq!(settings.stored.provider_name().as_deref(), Some("google"));
        assert_eq!(settings.stored.model.as_deref(), Some("m-a"));
        let rows = settings.instance_rows();
        assert_eq!(rows.len(), 2, "the google instance plus the add row");
        assert!(!rows[0].add);
        assert!(rows[0].active, "the only instance is the serving one");
        assert!(rows[1].add);
        assert!(!settings.workspace.is_empty(), "the About page names it");
    }

    #[test]
    fn settings_enter_on_the_add_row_opens_the_dialog() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Down to the add row, enter: the pop-in dialog comes up.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_some());
        // Esc closes the dialog, then leaves the settings for the chat.
        h.app.on_key(key(KeyCode::Esc));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_none());
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn settings_enter_on_an_instance_reactivates_it_and_lands_in_the_chat() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Enter on the highlighted instance re-activates it; with the
        // model stored, the session lands back in the chat.
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn settings_instance_activation_waits_for_a_running_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.turn.running = true;
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(h.app.screen, Screen::Settings(_)),
            "the switch waits for an idle session"
        );
        // The screen itself still answers: esc leaves.
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn settings_toggles_the_theme_live_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        assert_eq!(h.app.theme, crate::palette::DARK);
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::LIGHT, "the swap is live");
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
        // And back.
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::DARK);
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("dark"));
    }

    #[test]
    fn settings_toggles_the_icon_flavor_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('i')));
        assert_eq!(h.app.options.icons, crate::icons::Flavor::Nerd);
        assert!(
            std::ptr::eq(h.app.icons, crate::icons::Flavor::Nerd.set()),
            "the swap is live"
        );
        assert_eq!(h.store.load().unwrap().icons.as_deref(), Some("nerd"));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.icons_nerd, "the page follows the toggle");
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
        assert!(!h.app.turn.running, "no fake turn starts");
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
    fn slash_login_opens_the_providers_page_and_esc_returns_without_quitting() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.purpose, SettingsPurpose::FromChat);
        assert_eq!(settings.section, SettingsSection::Providers);
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(!h.app.should_quit, "esc goes back, not out");
    }

    #[test]
    fn esc_on_the_startup_settings_gate_quits() {
        let mut h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.should_quit, "no chat behind the gate: esc quits");
    }

    #[test]
    fn a_click_on_the_sidebar_switches_sections_and_a_row_acts() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Draw one frame so the renderer registers the hit rects, then
        // click the Appearance row (the sidebar's second section row).
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        let row_y = h
            .app
            .settings_hits
            .iter()
            .find_map(|(area, hit)| {
                matches!(
                    hit,
                    crate::app::SettingsHit::Sidebar(crate::screens::SettingsSection::Appearance)
                )
                .then_some(area.y)
            })
            .expect("the appearance row is clickable");
        h.app.on_click(4, row_y);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Appearance);
        // A click on a toggle row acts: the theme flips and persists.
        let (row_x, row_y) = h
            .app
            .settings_hits
            .iter()
            .find_map(|(area, hit)| {
                matches!(hit, crate::app::SettingsHit::PageRow(1)).then_some((area.x, area.y))
            })
            .expect("the theme row is clickable");
        h.app.on_click(row_x, row_y);
        assert_eq!(h.app.theme, crate::palette::LIGHT);
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
    }

    #[test]
    fn settings_cycles_the_palette_live_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        assert_eq!(h.app.theme, crate::palette::DARK);
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('c')));
        assert_eq!(
            h.app.options.colors,
            crate::palette::Palette::Aquamarine,
            "rhema steps to aquamarine"
        );
        assert_eq!(h.app.theme, crate::palette::AQUAMARINE_DARK, "live swap");
        assert_eq!(
            h.store.load().unwrap().colors.as_deref(),
            Some("aquamarine")
        );
        // The appearance carries over: the palette's light tokens show.
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::AQUAMARINE_LIGHT);
        // Cycling wraps back to rhema through every palette.
        for expected in [
            crate::palette::Palette::BlackWhite,
            crate::palette::Palette::Dracula,
            crate::palette::Palette::CatppuccinFrappe,
            crate::palette::Palette::CatppuccinMacchiato,
            crate::palette::Palette::CatppuccinMocha,
            crate::palette::Palette::Nord,
            crate::palette::Palette::Gruvbox,
            crate::palette::Palette::Solarized,
            crate::palette::Palette::M3You,
            crate::palette::Palette::Custom,
            crate::palette::Palette::Rhema,
        ] {
            h.app.on_key(key(KeyCode::Char('c')));
            assert_eq!(h.app.options.colors, expected);
        }
        assert_eq!(h.app.theme, crate::palette::LIGHT);
        assert_eq!(h.store.load().unwrap().colors.as_deref(), Some("rhema"));
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
        h.app.turn.running = true;
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
        assert!(!h.app.turn.running, "no turn starts");
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

    #[test]
    fn the_status_tracker_follows_the_turn_lifecycle() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut tracker = crate::status::Tracker::default();

        // At rest before anything runs.
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Idle
        ));

        // A turn runs: the step drives progress against the 4-step cap.
        h.app.turn.running = true;
        tracker.observe(&UiEvent::Engine(OwnedEvent::StepStart { iteration: 2 }));
        match tracker.status_for(&h.app) {
            crate::status::Status::Working {
                msg: None,
                progress: Some(progress),
            } => assert_eq!(progress, 50),
            _ => panic!("a running turn is working at its step fraction"),
        }

        // The latest tool call names the working state.
        tracker.observe(&UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        match tracker.status_for(&h.app) {
            crate::status::Status::Working {
                msg: Some(msg),
                progress: Some(50),
            } => assert_eq!(msg, "bash"),
            _ => panic!("the tool call is the working state's msg"),
        }

        // A pending approval blocks ahead of the running state.
        h.app.turn.pending_approval = Some(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::Value::Null,
            provider_data: serde_json::Value::Null,
        });
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Blocked { .. }
        ));

        // The turn ends in failure: the error stands at the idle prompt,
        // first line only.
        let agent = h
            .app
            .agent
            .take()
            .expect("the keyed harness built an agent");
        h.app.turn.pending_approval = None;
        h.app.turn.running = false;
        tracker.observe(&UiEvent::TurnFinished {
            agent: Box::new(agent),
            result: Err("provider exploded\nwith detail".to_owned()),
        });
        match tracker.status_for(&h.app) {
            crate::status::Status::Error { msg: Some(msg) } => {
                assert_eq!(msg, "provider exploded");
            }
            _ => panic!("a failed turn reads as an error at the idle prompt"),
        }

        // A later turn outranks the remembered failure and ends clean:
        // back to idle, not to the old error.
        h.app.turn.running = true;
        tracker.observe(&UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Working { .. }
        ));
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k")),
            CodingAgentConfig::new("m-a", std::env::temp_dir()),
        )
        .unwrap();
        h.app.turn.running = false;
        tracker.observe(&UiEvent::TurnFinished {
            agent: Box::new(agent),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        });
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Idle
        ));
    }
}
