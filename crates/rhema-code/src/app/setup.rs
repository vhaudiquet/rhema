//! What the runner fixed before the app started, and the vocabulary
//! the state machine speaks: the setup, the backend a profile
//! resolves to, the screens and effects, and the config helpers the
//! agent builds read.

use super::*;
use rhema_bash::runtime::{Feature, Kit, Needs, Shell, Stub};
use rhema_provider_registry::build;

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
    /// The file this run's conversation persists to when `--save` names
    /// none: `--resume`'s own file, or the fresh name the runner
    /// allocated under the sessions cache. Naming a file writes nothing,
    /// so a run that never speaks leaves no husk behind.
    pub checkpoint: Option<PathBuf>,
    /// The sessions cache `/clear` backs the outgoing conversation up
    /// into and allocates the next conversation's file under. `None`
    /// reads `sessions::base_dir()` (`$XDG_CACHE_HOME/rhema/sessions`);
    /// a test points it at a scratch directory so a clear never writes
    /// the machine's real cache.
    pub sessions_base: Option<PathBuf>,
    /// The startup flags.
    pub options: RunOptions,
    /// The secret store fresh logins place keys in and removed logins
    /// clean up after; `None` keeps every key in the file (tests, or a
    /// runner that opted out).
    pub secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
    /// The session's assembled sandbox, when the startup choice was
    /// non-local, built and seeded before the app existed; `None` is
    /// the local fast path, the agent's own tools.
    pub sandboxed: Option<crate::sandbox::Sandboxed>,
    /// The machine half of the sandbox availability probe, probed once
    /// before the app existed: the offered set re-derives from these
    /// facts per profile, at startup, and at every profile switch.
    pub sandbox_machines: crate::sandbox::SandboxMachines,
    /// The home the app resolves machine-global paths under, the
    /// isolation seam a test points at a scratch directory so the
    /// machine's own global definitions and skills never leak into a
    /// page. `None` reads the environment (`XDG_CONFIG_HOME`, then
    /// `HOME`, then `USERPROFILE` on windows).
    pub home: Option<PathBuf>,
    /// Run the first-run onboarding wizard: no config existed, or the
    /// last wizard opened but never finished. `rhema onboard` sets it
    /// too, whatever the config says.
    pub onboard: bool,
}

/// The model backend a session runs on: which provider, and whatever
/// credentials or endpoints it needs.
#[derive(Clone, PartialEq, Eq)]
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
        /// The instance's stored request shape, when it chose one;
        /// `None` lets the registry's route rule decide.
        api: Option<rhema_config::ProviderApi>,
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
    /// OpenCode Console or Go, by Console API key: an instance may name
    /// a base URL (`…/inference/openai/v1` for pay-as-you-go,
    /// `…/inference/go/openai/v1` for Go); `None` speaks to the
    /// pay-as-you-go route.
    OpenCode {
        /// The resolved key (`env` first, stored second).
        api_key: String,
        /// The instance's base URL, when it stored one.
        base_url: Option<String>,
        /// The conversation id every request carries. Kept across
        /// rebuilds of the same conversation.
        session_id: String,
    },
    /// GitHub Copilot, driven by the device flow's long-lived GitHub
    /// OAuth token (the adapter exchanges it for short-lived tokens).
    Copilot {
        /// The resolved GitHub token (`env` first, stored second).
        api_key: String,
    },
    /// A ChatGPT subscription, driven by the browser sign-in's OAuth
    /// credential (tokens and account facts, JSON-encoded).
    ChatGpt {
        /// The resolved credential (`env` first, stored second).
        credential: String,
    },
    /// Mistral's hosted API, by API key: an instance may name a base
    /// URL (a dedicated deployment); `None` speaks to the public API.
    Mistral {
        /// The resolved key (`env` first, stored second).
        api_key: String,
        /// The instance's base URL, when it stored one.
        base_url: Option<String>,
    },
    /// A Mistral Vibe subscription, by the API key its browser sign-in
    /// mints. The same wire as [`Self::Mistral`], a different plan.
    MistralVibe {
        /// The resolved key (`env` first, stored second).
        api_key: String,
        /// The instance's base URL, when it stored one.
        base_url: Option<String>,
    },
}

/// What a backend's credentials print as: the variant and endpoint
/// stay visible, the key does not.
const REDACTED: &str = "[redacted]";

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Google { .. } => f
                .debug_struct("Google")
                .field("api_key", &REDACTED)
                .finish(),
            Self::LlamaCpp { url, api_key } => f
                .debug_struct("LlamaCpp")
                .field("url", url)
                .field("api_key", &api_key.as_ref().map(|_| REDACTED))
                .finish(),
            Self::OpenRouter { .. } => f
                .debug_struct("OpenRouter")
                .field("api_key", &REDACTED)
                .finish(),
            Self::Anthropic { .. } => f
                .debug_struct("Anthropic")
                .field("api_key", &REDACTED)
                .finish(),
            Self::OpenAi { base_url, api, .. } => f
                .debug_struct("OpenAi")
                .field("api_key", &REDACTED)
                .field("base_url", base_url)
                .field("api", api)
                .finish(),
            Self::DeepSeek { .. } => f
                .debug_struct("DeepSeek")
                .field("api_key", &REDACTED)
                .finish(),
            Self::Zai { base_url, .. } => f
                .debug_struct("Zai")
                .field("api_key", &REDACTED)
                .field("base_url", base_url)
                .finish(),
            Self::OpenCode {
                base_url,
                session_id,
                ..
            } => f
                .debug_struct("OpenCode")
                .field("api_key", &REDACTED)
                .field("base_url", base_url)
                .field("session_id", session_id)
                .finish(),
            Self::Copilot { .. } => f
                .debug_struct("Copilot")
                .field("api_key", &REDACTED)
                .finish(),
            Self::ChatGpt { .. } => f
                .debug_struct("ChatGpt")
                .field("credential", &REDACTED)
                .finish(),
            Self::Mistral { base_url, .. } => f
                .debug_struct("Mistral")
                .field("api_key", &REDACTED)
                .field("base_url", base_url)
                .finish(),
            Self::MistralVibe { base_url, .. } => f
                .debug_struct("MistralVibe")
                .field("api_key", &REDACTED)
                .field("base_url", base_url)
                .finish(),
        }
    }
}

impl Backend {
    /// An OpenCode backend with a fresh conversation id. Model rebuilds
    /// within one conversation carry the id across through
    /// [`Backend::keep_session`].
    pub fn opencode(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self::OpenCode {
            api_key: api_key.into(),
            base_url: base_url
                .map(|url| url.trim().to_owned())
                .filter(|url| !url.is_empty()),
            session_id: rhema_provider_opencode::new_session_id(),
        }
    }

    /// Keeps this conversation's OpenCode id when a rebuild lands on the
    /// same kind, a model pick, a settings change, so Console keeps
    /// routing and caching one conversation consistently. A rebuild onto
    /// another provider, or from one, is a different route and keeps its
    /// own id.
    pub fn keep_session(&mut self, previous: &Backend) {
        if let (
            Backend::OpenCode { session_id, .. },
            Backend::OpenCode {
                session_id: previous_id,
                ..
            },
        ) = (&mut *self, previous)
        {
            *session_id = previous_id.clone();
        }
    }

    /// The provider implementation for this backend: the raw adapter
    /// under the shared retry, so a transient failure costs a pause
    /// instead of the turn (see `rhema_provider::Retrying`).
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::new(rhema_provider::Retrying::new(self.wired().provider))
    }

    /// The kind's adapters, from the variant's own credentials, the
    /// enum's job is holding the credential shape, construction is the
    /// registry's. A build failure would mean a variant whose shape
    /// lies (a keyed backend without a key), which the enum cannot
    /// represent; route overrides ride the variant's stored base URL,
    /// blank reading as unset.
    fn wired(&self) -> rhema_provider_registry::Built {
        match self {
            Backend::Google { api_key } => build("google", Some(api_key), None, None, None),
            Backend::LlamaCpp { url, api_key } => {
                build("llama-cpp", api_key.as_deref(), Some(url), None, None)
            }
            Backend::OpenRouter { api_key } => build("openrouter", Some(api_key), None, None, None),
            Backend::Anthropic { api_key } => build("anthropic", Some(api_key), None, None, None),
            Backend::OpenAi {
                api_key,
                base_url,
                api,
            } => build(
                "openai",
                Some(api_key),
                None,
                base_url.as_deref(),
                api.map(rhema_config::ProviderApi::as_str),
            ),
            Backend::DeepSeek { api_key } => build("deepseek", Some(api_key), None, None, None),
            Backend::Zai { api_key, base_url } => {
                build("zai", Some(api_key), None, base_url.as_deref(), None)
            }
            Backend::OpenCode {
                api_key,
                base_url,
                session_id,
            } => Ok(rhema_provider_registry::build_opencode(
                api_key,
                base_url.as_deref(),
                Some(session_id),
            )),
            Backend::Copilot { api_key } => build("copilot", Some(api_key), None, None, None),
            Backend::ChatGpt { credential } => build("chatgpt", Some(credential), None, None, None),
            Backend::Mistral { api_key, base_url } => {
                build("mistral", Some(api_key), None, base_url.as_deref(), None)
            }
            Backend::MistralVibe { api_key, base_url } => build(
                "mistral-vibe",
                Some(api_key),
                None,
                base_url.as_deref(),
                None,
            ),
        }
        .expect("a Backend's own credentials build")
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
            Backend::OpenCode { .. } => "opencode",
            Backend::Copilot { .. } => "copilot",
            Backend::ChatGpt { .. } => "chatgpt",
            Backend::Mistral { .. } => "mistral",
            Backend::MistralVibe { .. } => "mistral-vibe",
        }
    }

    /// Builds the backend a profile's settings call for; `None` when the
    /// profile names no provider (or a spelling no provider answers to,
    /// or has no usable credentials for the one it names), the login
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
                // store": say so, or an unreadable store reads as a key
                // the user never stored.
                Some(store) if !store.reachable() => format!(
                    "{help}, the secret store is unavailable or its key is missing, \
                     so the stored keys were unreadable"
                ),
                _ => help.to_owned(),
            }
        };
        Self::from_kind(&active, profile, secrets).ok_or_else(missing)
    }

    /// The backend one provider kind resolves to, instance first: a
    /// stored instance carries its endpoint and request shape, the
    /// environment-alone path fills in defaults. The model picker and
    /// the subagent factory both read this, so a cross-provider
    /// subagent reaches the same proxy or local server the session
    /// would.
    pub(crate) fn from_kind(
        kind: &str,
        profile: &rhema_config::Profile,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Option<Backend> {
        if let Some((_, instance)) = profile.instance_for_provider(kind) {
            return Self::from_instance(profile, instance, secrets);
        }
        Self::named(kind, profile, secrets)
    }

    /// Builds one backend per logged-in instance of the profile, the
    /// active one first, then the rest in name order, so the picker
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
    /// carries one. A kind no provider answers to builds nothing, an
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
                        api: instance.api,
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
            "opencode" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::opencode(api_key, profile.instance_url(instance))),
            "copilot" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Copilot { api_key }),
            "chatgpt" => profile
                .instance_api_key(instance, secrets)
                .map(|credential| Backend::ChatGpt { credential }),
            "mistral" => {
                profile
                    .instance_api_key(instance, secrets)
                    .map(|api_key| Backend::Mistral {
                        api_key,
                        base_url: profile.instance_url(instance),
                    })
            }
            "mistral-vibe" => {
                profile
                    .instance_api_key(instance, secrets)
                    .map(|api_key| Backend::MistralVibe {
                        api_key,
                        base_url: profile.instance_url(instance),
                    })
            }
            "google" => profile
                .instance_api_key(instance, secrets)
                .map(|api_key| Backend::Google { api_key }),
            // A kind no row answers to builds nothing, an unknown
            // spelling never reads as google.
            _ => None,
        }
    }

    /// The backend for one provider's config name from the environment
    /// alone, `None` when the environment carries no usable credential
    /// for it. Profiles with a stored instance resolve through
    /// [`Backend::from_profile`] instead.
    pub(crate) fn named(
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
                    api: None,
                }),
            "deepseek" => profile
                .api_key("deepseek", secrets)
                .map(|api_key| Backend::DeepSeek { api_key }),
            "zai" => profile.api_key("zai", secrets).map(|api_key| Backend::Zai {
                api_key,
                base_url: None,
            }),
            "opencode" => profile
                .api_key("opencode", secrets)
                .map(|api_key| Backend::opencode(api_key, None)),
            "copilot" => profile
                .api_key("copilot", secrets)
                .map(|api_key| Backend::Copilot { api_key }),
            "chatgpt" => profile
                .api_key("chatgpt", secrets)
                .map(|credential| Backend::ChatGpt { credential }),
            "mistral" => profile
                .api_key("mistral", secrets)
                .map(|api_key| Backend::Mistral {
                    api_key,
                    base_url: None,
                }),
            "mistral-vibe" => {
                profile
                    .api_key("mistral-vibe", secrets)
                    .map(|api_key| Backend::MistralVibe {
                        api_key,
                        base_url: None,
                    })
            }
            _ => None,
        }
    }

    /// The backend's subscription allowance, when its provider reports
    /// one: prepaid credit on OpenRouter and DeepSeek, the seat's
    /// premium requests on Copilot, the Coding Plan's 5-hour and weekly
    /// windows on Z.ai, the Vibe plan's monthly budget on Mistral.
    /// `Ok(None)` for the providers with no such endpoint, the footer
    /// just never shows their badge.
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
                    .with_base_url(rhema_provider_openai_compatible::DEEPSEEK_BASE_URL)
                    .balance()
                    .await
                    .map(Some)
                    .map_err(|err| err.to_string())
            }
            Backend::Zai { api_key, base_url } => {
                let mut provider = rhema_provider_anthropic::AnthropicProvider::zai(api_key);
                if let Some(base_url) = base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                {
                    provider = provider.with_base_url(base_url.to_owned());
                }
                provider.allowance().await.map_err(|err| err.to_string())
            }
            Backend::Mistral { api_key, base_url } => {
                mistral_allowance(api_key, base_url.as_deref(), false).await
            }
            Backend::MistralVibe { api_key, base_url } => {
                mistral_allowance(api_key, base_url.as_deref(), true).await
            }
            // No allowance endpoint to ask: Google keys, local servers,
            // Anthropic, the generic OpenAI-compatible family.
            Backend::Google { .. }
            | Backend::LlamaCpp { .. }
            | Backend::Anthropic { .. }
            | Backend::OpenAi { .. }
            | Backend::OpenCode { .. }
            | Backend::ChatGpt { .. } => Ok(None),
        }
    }

    /// The models the backend offers, normalized to the picker's row
    /// type. The listing rides the registry's catalog seam, off the
    /// retry: it answers or it fails.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, rhema_core::ProviderError> {
        self.wired().listing.list_models().await
    }
}

/// The Mistral plan's monthly Vibe Code budget, read off the same
/// provider the backend completes with, the stored instance's base URL
/// honored the same way. A key without a Vibe subscription, or a member
/// the routes stay hidden from, answers `Ok(None)`: no badge, never a
/// wrong figure.
async fn mistral_allowance(
    api_key: &str,
    base_url: Option<&str>,
    vibe: bool,
) -> Result<Option<Allowance>, String> {
    let api_key = api_key.to_owned();
    let mut provider = if vibe {
        rhema_provider_mistral::MistralProvider::vibe(api_key)
    } else {
        rhema_provider_mistral::MistralProvider::new(api_key)
    };
    if let Some(base_url) = base_url.map(str::trim).filter(|url| !url.is_empty()) {
        provider = provider.with_base_url(base_url.to_owned());
    }
    provider.allowance().await.map_err(|err| err.to_string())
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
    /// What paints behind the ink: the palette's fills or the
    /// terminal's own background.
    pub background: crate::palette::Background,
    /// The M3 You seed, `0xRRGGBB`; `None` is Material's baseline
    /// purple. Every palette but M3 You ignores it.
    pub m3_source: Option<u32>,
    /// The custom palette's token overrides; every palette but Custom
    /// ignores it.
    pub custom: Option<crate::store::CustomPalette>,
    /// Which icon flavor the footer draws with.
    pub icons: crate::icons::Flavor,
    /// The input caret's shape.
    pub cursor: crate::cursor::CursorStyle,
    /// Whether the transcript draws its scrollbar in the right margin.
    pub scrollbar: bool,
    /// The config profile this session runs under.
    pub profile: Option<String>,
    /// Workspace root for tools.
    pub workspace: PathBuf,
    /// Max steps per turn.
    pub max_steps: std::num::NonZeroU32,
    /// Send the whole history every step, skipping the transcript's
    /// bounded request view.
    pub no_transcript: bool,
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
    /// The resolved design tokens: the palette's appearance with the
    /// background choice painted on, the one theme the screens read.
    /// Every resolution site (startup, the live toggles) lands here, so
    /// a background flip and a palette flip can never disagree.
    pub(crate) fn resolved_theme(&self) -> crate::palette::Theme {
        self.colors
            .theme(self.theme, self.palette_inputs())
            .painted(self.background)
    }

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
/// `Screen` (the same reason the login it absorbed was boxed once). The
/// onboarding wizard is boxed for the same reason, its dialog, model
/// rows, and skills snapshot together.
pub enum Screen {
    /// The first-run wizard: logo rail, steps, and the finish that
    /// marks the config onboarded.
    Onboarding(Box<OnboardingScreen>),
    /// Choose a model.
    Picker(Box<PickerScreen>),
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
    /// A category row of the add-instance dialog.
    DialogCategory(usize),
    /// A provider row of the add-instance dialog.
    DialogProvider(usize),
    /// A request-shape row of the add-instance dialog.
    DialogApiChoice(usize),
    /// The running login flow's open button.
    DialogFlowOpen,
    /// The running login flow's copy button.
    DialogFlowCopy,
}

/// What a left click on the onboarding wizard landed on. The renderer
/// registers the rects every frame; the app matches the position of
/// the click against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingHit {
    /// A rail row: jump to this step.
    RailStep(crate::onboarding::OnboardingStep),
}

/// A side effect the runner must perform (network or task spawn).
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Fetch the model list for `why`.
    FetchModels(FetchWhy),
    /// Fetch the active backend's subscription allowance, if its
    /// provider reports one.
    FetchUsage,
    /// Read the clipboard for the paste key off the UI thread; the
    /// grab lands back as a `UiEvent::ClipboardGrabbed`.
    GrabClipboard,
    /// Start one turn with this prompt: the text, the paste spans it
    /// records, and the images beside it, in the order they were
    /// submitted.
    StartTurn {
        /// The prompt's text, payloads in place.
        text: String,
        /// The images beside the text.
        images: Vec<Image>,
        /// The paste spans the message records, in prompt order.
        spans: Vec<crate::paste::PasteSpan>,
    },
    /// Re-run the failed turn: no new prompt, the same request goes
    /// back out (`Prompt::Retry`). The app marks the turn running and
    /// clears the error strip before this fires.
    RetryTurn,
    /// A steered prompt interrupted the running turn; the status strip
    /// names it until the next tool call takes over.
    SteerTurn(String),
    /// Spawn the interactive login flow the login screen chose (its
    /// stage is already `Flow`); updates arrive as
    /// `UiEvent::LoginUpdate`.
    StartLoginFlow,
    /// Stand the sandbox picker's choice up, container, connection,
    /// seeding, through the shared assembly. Reports back as
    /// `UiEvent::SandboxAssembled`; the session keeps its current tools
    /// until then.
    AssembleSandbox(SandboxChoice),
    /// Load this saved session off-thread for `/resume`; the parts
    /// land as `UiEvent::SessionResumed` and the app applies them.
    ResumeSession(PathBuf),
    /// List this workspace's saved sessions off-thread; every file is
    /// read and parsed for its opening prompt, and the picker must
    /// paint before that runs. Lands as `UiEvent::SessionsListed`.
    ListSessions {
        /// The session cache root.
        base: PathBuf,
        /// The workspace whose sessions list.
        workspace: PathBuf,
        /// The request the listing answers, so a listing a newer open
        /// superseded lands stale and drops.
        request: u64,
    },
    /// Open this `SUBAGENT.md` in `$EDITOR`: the runner suspends the
    /// TUI around the blocking editor and re-enters after, and the
    /// Subagents page's rows refresh then, the edit happened behind
    /// the app's back.
    EditSubagent(PathBuf),
    /// Fetch the distribution channel's latest version off-thread;
    /// the answer lands as `UiEvent::UpdateChecked`.
    CheckForUpdates,
    /// Run the install script off-thread; the outcome lands as
    /// `UiEvent::UpdateInstalled`.
    InstallUpdate,
    /// Write this caret shape to the terminal now: the Appearance
    /// page's cycle is live, like the theme and palette flips.
    SetCursorStyle(crate::cursor::CursorStyle),
}

/// The Subagents page's rows: every discovered definition with its
/// provenance and the file an edit opens. The source is the file check
/// discovery shadows by, a workspace definition outranks a global one
/// of the same name, and a definition of the built-in's name replaces
/// the built-in wholesale. `disabled` is the config's off switch: off
/// rows still list, with `enabled` false; only discovery filters them.
pub(crate) fn subagent_rows(
    global: Option<&Path>,
    workspace: &Path,
    disabled: &std::collections::BTreeSet<String>,
) -> Vec<SubagentRow> {
    rhema_subagents::discover(global, workspace)
        .into_iter()
        .map(|def| {
            let workspace_file = workspace
                .join(".rhema")
                .join("subagents")
                .join(&def.name)
                .join(rhema_subagents::def::FILE);
            let (source, path) = if workspace_file.is_file() {
                (SubagentSource::Workspace, Some(workspace_file))
            } else {
                match global.map(|dir| dir.join(&def.name).join(rhema_subagents::def::FILE)) {
                    Some(file) if file.is_file() => (SubagentSource::Global, Some(file)),
                    _ => (SubagentSource::Builtin, None),
                }
            };
            SubagentRow {
                enabled: !disabled.contains(&def.name),
                name: def.name,
                description: def.description,
                model: def.model,
                max_steps: def.max_steps,
                source,
                path,
            }
        })
        .collect()
}

/// The skills page's rows (and the wizard's skills step's): every
/// discovered skill with its source, the tier discovery stamped on it,
/// a global skill of a bundled name reading as bundled, and its on/off
/// state from the config's disabled set.
pub(crate) fn skill_rows(
    agents: Option<&Path>,
    global: Option<&Path>,
    workspace: &Path,
    disabled: &std::collections::BTreeSet<String>,
) -> Vec<SkillRow> {
    rhema_skills::discover(agents, global, workspace)
        .into_iter()
        .map(|skill| {
            let source = match skill.tier {
                rhema_skills::SkillTier::Workspace => SkillSource::Workspace,
                rhema_skills::SkillTier::Global if crate::bundled::is_bundled(&skill.name) => {
                    SkillSource::Bundled
                }
                rhema_skills::SkillTier::Global => SkillSource::Global,
                rhema_skills::SkillTier::WorkspaceAgents | rhema_skills::SkillTier::HomeAgents => {
                    SkillSource::Agents
                }
            };
            let enabled = !disabled.contains(&skill.name);
            SkillRow {
                name: skill.name,
                description: skill.description,
                license: skill.license,
                source,
                enabled,
            }
        })
        .collect()
}

/// Writes the built-in definition into the global directory, producing
/// the file an edit of the built-in row opens. An existing file is
/// left alone: it is a shadow that discovery already prefers.
pub(crate) fn shadow_builtin(global: &Path, name: &str) -> std::io::Result<PathBuf> {
    let builtin = rhema_subagents::builtin();
    let folder = global.join(name);
    std::fs::create_dir_all(&folder)?;
    let file = folder.join(rhema_subagents::def::FILE);
    if !file.exists() {
        // The description is a YAML scalar: double-quoted, with the
        // escapes a double-quoted scalar honors.
        let description = builtin
            .description
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(
            &file,
            format!(
                "---\ndescription: \"{description}\"\n---\n\n{}\n",
                builtin.body
            ),
        )?;
    }
    Ok(file)
}

fn agent_config(
    provider: Arc<dyn ModelProvider>,
    options: &RunOptions,
    model: Option<String>,
    session_id: Option<&str>,
    disabled_skills: &std::collections::BTreeSet<String>,
    disabled_subagents: &std::collections::BTreeSet<String>,
) -> HostAgentConfig {
    HostAgentConfig {
        max_turn_steps: options.max_steps,
        transcript: (!options.no_transcript).then(rhema_transcript::TranscriptSettings::default),
        thinking: options.thinking,
        base_system: options.base_system.clone(),
        disabled_skills: disabled_skills.clone(),
        disabled_subagents: disabled_subagents.clone(),
        // The conversation's own key rides the OpenRouter dialect's
        // sticky-routing `session_id`; the checkpoint id is stable
        // across a restart, so a resumed conversation keeps its bucket
        // and its warm provider cache.
        session_id: session_id.map(str::to_owned),
        // Approval is intentionally `AllowAll`: the sandbox, not a
        // prompt, is the isolation boundary. The approval modal is a
        // seam for a future policy.
        ..HostAgentConfig::new(
            provider,
            model.unwrap_or_default(),
            &options.workspace,
            rhema_host_agent::AllowAll,
        )
    }
}

/// The host-agent config a session runs on: the base config, the
/// session's assembled sandbox applied when it runs on one, and the
/// subagent wire. The application order is the shared assembly's: the
/// sandbox tools and seeded skills land BEFORE the wire, the
/// subagents' inner shell inherits the executor
/// (`ShellTools::executor()`), their definition discovery stays
/// host-side, and the sandbox-side workspace path lands AFTER it.
/// The prompt sections land before the wire too: the subagents stamp
/// the config's system-prompt ingredients at wire time, so they run
/// under the same environment facts and user preferences the parent
/// sends. Returns the wire's handle beside the config: the
/// registry-backed status subscription the status strip renders one
/// live line per background subagent job from.
///
/// `jobs_steering` is the background-shell poller's finish feed, when
/// the process spawned one: the config has one `steering` slot and two
/// feeds reach for it, so the wire's registry and this chain through
/// `SteeringChain`, subagent finishes first, job finishes second, one
/// message either way.
///
/// The third return element is the probed tools this machine misses,
/// for the UI's startup notice, the user's view of the same probe
/// that stamped the prompt's environment section. Re-derives per
/// assembly, so a profile switch that changes nothing still re-answers
/// the same truth; a sandboxed session answers empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn session_agent_config(
    mcp: &crate::mcp_config::McpStore,
    kits_registry: &crate::kits::Registry,
    options: &RunOptions,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
    model: Option<String>,
    provider: Arc<dyn ModelProvider>,
    disabled_skills: &std::collections::BTreeSet<String>,
    disabled_subagents: &std::collections::BTreeSet<String>,
    profile: &rhema_config::Profile,
    session_key: &str,
    session_id: Option<&str>,
    secrets: Option<&Arc<dyn rhema_config::secrets::SecretStore>>,
    jobs_steering: Option<Arc<dyn rhema_host_agent::Steering>>,
    jobs_tag: &str,
) -> (
    HostAgentConfig,
    rhema_subagents::SubagentHandle,
    Vec<String>,
) {
    // The caller builds the feed for the daemon it actually has: the
    // host's event stream locally, the sandbox's listing remotely.
    let mut config = agent_config(
        provider.clone(),
        options,
        model,
        session_id,
        disabled_skills,
        disabled_subagents,
    );
    // The subagent tool's cross-provider `model` spellings resolve
    // through the same per-kind answers the model picker's catalog
    // reads: one backend per kind, instance first, so a stored proxy
    // or local endpoint is not silently replaced by the vendor
    // default. Built here where `Backend::from_kind` lives; `None` (no
    // secret store) leaves the session's provider the only one, and the
    // tool's spec says so.
    config.subagent_providers = secrets.map(|secrets| {
        let profile = profile.clone();
        let secrets = secrets.clone();
        let factory: rhema_host_agent::SubagentProviders = Arc::new(move |kind: &str| {
            Backend::from_kind(kind, &profile, Some(secrets.as_ref()))
                .map(|backend| backend.provider())
        });
        factory
    });
    if let Some(sandboxed) = sandboxed {
        sandboxed.apply_tools(&mut config);
    }
    let missing_tools = apply_prompt_sections(&mut config, options, sandboxed);
    // Every kit this session places shares one init file: the stub
    // text is static, the per-session values are env. A mounted sandbox
    // reads the file from the shared root; a sandbox with no root reads
    // the static file from its own cache, and the per-session state
    // lives under the same cache.
    let inherited = std::env::var_os("BASH_ENV");
    let mut kits = Vec::new();
    let mut sections: std::collections::BTreeMap<Feature, &'static str> =
        std::collections::BTreeMap::new();
    let key = crate::shared_kit::runtime_session_key(session_key);
    let shared_kits = sandboxed
        .and_then(|sandboxed| sandboxed.shared())
        .map(|shared| crate::shared_kit::wire(shared, &key, jobs_tag));
    let remote = sandboxed.and_then(|sandboxed| sandboxed.remote());
    let jobs_ephemeral =
        sandboxed.is_some_and(|sandboxed| sandboxed.choice == crate::args::SandboxChoice::Ecrou);

    let jobs = if sandboxed.is_some() {
        if jobs_ephemeral {
            // A job's gob daemon dies with the boot; every command is
            // a fresh machine, so `job run` would always fail.
            None
        } else if let Some(remote) = remote {
            remote.gob.then(|| {
                let mut env = std::collections::BTreeMap::new();
                env.insert(
                    rhema_bash::jobs::VAR_SESSION.to_owned(),
                    jobs_tag.to_owned(),
                );
                env.insert(
                    rhema_bash::jobs::VAR_SESSION_FILE.to_owned(),
                    remote.jobs_session(&key),
                );
                (env, rhema_bash::jobs::PROMPT_SECTION)
            })
        } else {
            shared_kits
                .as_ref()
                .and_then(|kits| kits.jobs.clone())
                .map(|wiring| (wiring.shell_env.clone(), wiring.section))
        }
    } else {
        rhema_bash::jobs::wire(None, jobs_tag)
            .cloned()
            .map(|wiring| (wiring.shell_env.clone(), wiring.section))
    };
    if let Some((env, section)) = jobs {
        // A sandbox drives `gob` from the init file: the helper binary
        // is not installed for it. The host keeps the hidden mode, where
        // the running binary is already there.
        if sandboxed.is_some() {
            kits.push(kit_for_shell(
                Feature::Jobs,
                env,
                jobs_shell(),
                Needs::MACRO,
            ));
        } else {
            kits.push(kit_for(
                Feature::Jobs,
                env,
                vec![builtin_stub(
                    rhema_bash::jobs::NAME,
                    rhema_bash::jobs::VAR_BIN,
                    vec![rhema_bash::jobs::MODE],
                )],
                Needs::MACRO,
            ));
        }
        sections.insert(Feature::Jobs, section);
    }

    let vision = if sandboxed.is_some() {
        if let Some(remote) = remote {
            let mut env = std::collections::BTreeMap::new();
            env.insert(
                rhema_bash::vision::VAR_STAGING.to_owned(),
                remote.vision_staging(&key),
            );
            Some((env, rhema_bash::vision::PROMPT_SECTION))
        } else {
            shared_kits
                .as_ref()
                .and_then(|kits| kits.vision.clone())
                .map(|wiring| (wiring.shell_env.clone(), wiring.section))
        }
    } else {
        rhema_bash::vision::wire(None)
            .cloned()
            .map(|wiring| (wiring.shell_env.clone(), wiring.section))
    };
    if let Some((env, section)) = vision {
        kits.push(kit_for_shell(
            Feature::Vision,
            env,
            vision_shell(),
            Needs::SHELL_STATE,
        ));
        sections.insert(Feature::Vision, section);
    }

    let search = crate::search_wire::wire_for_session(
        session_key,
        profile,
        secrets.map(|secrets| secrets.as_ref()),
        sandboxed,
        None,
    );
    if let Some(wiring) = &search {
        kits_registry.keep(wiring.clone());
    }
    let search = search.map(|wiring| (wiring.shell_env.clone(), wiring.section));
    if let Some((env, section)) = search {
        kits.push(kit_for_shell(
            Feature::Search,
            env,
            search_shell(),
            Needs::HOST_CALL,
        ));
        sections.insert(Feature::Search, section);
    }

    let decision = crate::decision_wire::wire_for_session(
        session_key,
        profile,
        secrets.map(|secrets| secrets.as_ref()),
        sandboxed,
        None,
    );
    if let Some(wiring) = &decision {
        kits_registry.keep(wiring.clone());
    }
    let decision = decision.map(|wiring| (wiring.shell_env.clone(), wiring.section));
    if let Some((env, section)) = decision {
        kits.push(kit_for_shell(
            Feature::Decision,
            env,
            decision_shell(),
            Needs::HOST_CALL,
        ));
        sections.insert(Feature::Decision, section);
    }

    let placed_env = if kits.is_empty() {
        None
    } else if let Some(shared) = sandboxed.and_then(|s| s.shared()) {
        match rhema_sandbox::kit::place_shared(shared, &key, &kits) {
            Ok(placed) => {
                if let Some(sandboxed) = sandboxed {
                    sandboxed.keep_session(placed.session);
                }
                Some(placed.env)
            }
            Err(err) => {
                eprintln!("warning: the sandbox kit placement failed: {err}");
                None
            }
        }
    } else if let Some(remote) = remote {
        let env = crate::sandbox::remote_kit_env(
            remote,
            &key,
            sections.contains_key(&Feature::Jobs),
            sections.contains_key(&Feature::Vision),
            &kits,
        );
        if let Some(sandboxed) = sandboxed {
            sandboxed.mark_remote_session(remote.session_dir(&key));
        }
        Some(env)
    } else {
        kits_registry
            .place(&kits, inherited.as_deref())
            .map(|placed| placed.env.clone())
    };
    if let Some(env) = placed_env {
        config.shell_env = env;
        if let Some(section) = sections.get(&Feature::Jobs) {
            config.jobs = Some((*section).to_owned());
        }
        if let Some(section) = sections.get(&Feature::Vision) {
            config.vision = Some((*section).to_owned());
        }
        if let Some(section) = sections.get(&Feature::Search) {
            config.websearch = Some((*section).to_owned());
        }
        if let Some(section) = sections.get(&Feature::Decision) {
            config.decision = Some((*section).to_owned());
        }
        if sections.contains_key(&Feature::Vision)
            && let Some(tools) = config.tools.as_deref()
        {
            if let Some(remote) = remote {
                let sandbox = sandboxed.and_then(|sandboxed| sandboxed.sandbox.clone());
                if let Some(sandbox) = sandbox {
                    tools.set_vision_inbox(None);
                    tools.set_vision_pull(Some(Arc::new(crate::sandbox::SandboxVisionPull {
                        sandbox,
                        workspace: sandboxed
                            .as_ref()
                            .map(|sandboxed| sandboxed.workspace.clone())
                            .unwrap_or_default(),
                        staging: remote.vision_staging(&key),
                    })));
                }
            } else {
                // The env names the sandbox's staging path; the host
                // attaches from the same directory through its own path.
                tools.set_vision_inbox(
                    config
                        .shell_env
                        .get(rhema_bash::vision::VAR_STAGING)
                        .map(std::path::PathBuf::from),
                );
            }
        }
    }
    config.shell_env.extend(mcp.env.clone());
    config.mcp = mcp.prompt.clone();
    let (mut config, subagents) = rhema_subagents::wire(config);
    // One steering slot, two feeds: the subagent registry drains
    // first, the background-shell feed second, so the announcements
    // coalesce into one message in a stable order. Either alone rides
    // as itself.
    config.steering = match (config.steering, jobs_steering) {
        (Some(subagents), Some(jobs)) => {
            Some(Arc::new(rhema_host_agent::SteeringChain::new(vec![
                subagents, jobs,
            ])))
        }
        (steering, None) => steering,
        (None, Some(jobs)) => Some(jobs),
    };
    if let Some(sandboxed) = sandboxed {
        sandboxed.apply_workspace(&mut config);
    }
    (config, subagents, missing_tools)
}

/// The built-in kit stub: the names are constants, so an invalid one
/// is a bug in this tree, not a session's input.
pub(crate) fn builtin_stub(name: &str, bin_var: &str, args: Vec<&str>) -> Stub {
    Stub::new(name, bin_var, args.into_iter().map(str::to_owned).collect())
        .expect("the built-in kit stub is a valid bash identifier")
}

/// One kit from a wiring's env: `BASH_ENV` is the placement's, and the
/// inherited file is baked into the init text, not carried as a var.
pub(crate) fn kit_for(
    feature: Feature,
    env: std::collections::BTreeMap<String, String>,
    stubs: Vec<Stub>,
    needs: Needs,
) -> Kit {
    let vars = env
        .into_iter()
        .filter(|(name, _)| name != "BASH_ENV" && name != "RHEMA_INHERITED_BASH_ENV")
        .collect();
    Kit::new(feature, stubs, needs).with_vars(vars)
}

/// The same kit with one shell macro and no helper stub.
pub(crate) fn kit_for_shell(
    feature: Feature,
    env: std::collections::BTreeMap<String, String>,
    shell: Shell,
    needs: Needs,
) -> Kit {
    kit_for(feature, env, Vec::new(), needs).with_shell(shell)
}

/// The `job` macro for a sandbox: `gob` and `jq`, no helper process.
pub(crate) fn jobs_shell() -> Shell {
    rhema_bash::jobs::shell()
}

/// The `see` macro, the one kit whose body is shell text.
pub(crate) fn vision_shell() -> Shell {
    Shell::new(rhema_bash::vision::NAME, rhema_bash::vision::SHELL_BODY)
        .expect("the vision macro is valid shell")
}

/// The `search` macro's host-endpoint body.
pub(crate) fn search_shell() -> Shell {
    Shell::new(
        rhema_bash::websearch::NAME,
        rhema_bash::websearch::SHELL_BODY,
    )
    .expect("the search macro is valid shell")
}

/// The `decide` macro's host-endpoint body.
pub(crate) fn decision_shell() -> Shell {
    Shell::new(rhema_bash::decision::NAME, rhema_bash::decision::SHELL_BODY)
        .expect("the decide macro is valid shell")
}

/// The prompt's two optional sections, through the one gate the
/// `system show` preview also reads, so what is previewed is what
/// turns send. The sandboxed session carries its kind's notice; a
/// local one probes the host. Runs BEFORE the subagent wire, so the
/// subagents stamp the same sections into their own prompts: the
/// sandbox-side path reads off the assembly, not the config, whose
/// workspace only becomes that path after the wire. Returns the
/// probe's absence list for the UI to surface, the user's view of
/// the same run, empty for a sandbox, whose host's gaps it does not
/// inherit.
fn apply_prompt_sections(
    config: &mut HostAgentConfig,
    options: &RunOptions,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
) -> Vec<String> {
    let kind = sandboxed.map(|sandboxed| sandboxed.choice.stored_name());
    let mounted = sandboxed
        .map(|sandboxed| sandboxed.workspace.clone())
        .unwrap_or_else(|| config.workspace.display().to_string());
    let (environment, user, missing) = crate::run::prompt_sections(
        &options.workspace,
        &mounted,
        kind,
        &config.model,
        crate::run::HARNESS,
    );
    config.environment = environment;
    config.user_preferences = user;
    missing
}

/// The context a reported usage stands for: the whole prompt plus the
/// reply, the shape of the next request. The adapters normalize
/// `input_tokens` to the inclusive convention, the cached subset
/// included, so the prompt total describes the conversation, never
/// just the tokens a cache miss had to re-read.
pub(crate) fn context_tokens(usage: &Usage) -> u64 {
    u64::from(usage.input_tokens) + u64::from(usage.output_tokens)
}

/// A rough token estimate over a history: about four characters per
/// token, counted across text, tool arguments, tool results, and image
/// data. There
/// is no tokenizer this side of the wire, and the footer is a gauge,
/// not an invoice.
pub(crate) fn estimate_history_tokens(history: &[Message]) -> u64 {
    // Images ride the wire as base64 but price as vision tokens, not
    // text: bytes-over-four priced one screenshot at ~260k fake
    // tokens. A flat per-image figure keeps the gauge within sight of
    // a real request's shape.
    const IMAGE_TOKENS: u64 = 1_000;
    let mut chars = 0usize;
    let mut images = 0u64;
    for message in history {
        for content in &message.content {
            match content {
                Content::Text(text) => chars += text.len(),
                Content::ToolCall(call) => chars += call.args.to_string().len(),
                Content::ToolResult(result) => chars += result.content.len(),
                Content::Image(_) => images += 1,
            }
        }
    }
    (chars as u64 / 4) + (images * IMAGE_TOKENS)
}

/// The profile a session runs on when no `--profile` names one: the
/// profile this workspace last served a turn under while it still can,
/// else the default profile while it can serve turns, otherwise the
/// first stored profile that can (the registry's order), a logged-in
/// profile must not open the login screen. The workspace memory walks
/// ancestors ([`crate::store::Document::last_profile_in`]), so a
/// profile chosen once in a folder reaches all its subfolders until one
/// chooses its own. Isolation still holds: this only picks the section
/// up front, and nothing reads across profiles afterwards. An explicit
/// flag never reaches here.
pub(crate) fn startup_profile(
    config: &crate::store::Config,
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

/// The sandbox a session runs `bash` on when nothing was picked yet:
/// the kind the profile stores (or `RHEMA_SANDBOX` names, the same
/// env-wins layering [`crate::sandbox::resolve_choice`] reads) while
/// this machine offers it, else the kind this workspace last ran while
/// it is still offered, else `local`. The machine gates every tier: a
/// stored docker kind with no docker daemon falls through to what the
/// workspace remembers, never stands a failed container up at startup.
/// Isolation still holds: this only picks the kind up front, and the
/// assembly reads the profile itself afterwards. An explicit pick
/// never reaches here.
pub(crate) fn startup_sandbox(
    config: &crate::store::Config,
    profile: &rhema_config::Profile,
    workspace: &Path,
    offered: &crate::sandbox::SandboxAvailability,
) -> SandboxChoice {
    let ready = |choice: SandboxChoice| offered.offers(choice);
    if let Some(choice) = SandboxChoice::from_stored(profile.sandbox_kind().as_deref())
        .filter(|choice| ready(*choice))
    {
        return choice;
    }
    if let Some(choice) = config
        .last_sandbox_in(workspace)
        .as_deref()
        .and_then(|kind| SandboxChoice::from_stored(Some(kind)))
        .filter(|choice| ready(*choice))
    {
        return choice;
    }
    // Local is always offered: the fallback needs no probe.
    SandboxChoice::Local
}

/// The sandbox the profile itself runs `bash` on: its own stored kind
/// (`RHEMA_SANDBOX` included, [`crate::sandbox::resolve_choice`]'s
/// env-wins layering), gated by what this machine offers the profile,
/// else local. The workspace's remembered kind is deliberately not a
/// tier here: a profile's sandbox is the profile's, never another
/// profile's workspace memory.
pub(crate) fn profile_sandbox(
    profile: &rhema_config::Profile,
    offered: &crate::sandbox::SandboxAvailability,
) -> SandboxChoice {
    SandboxChoice::from_stored(profile.sandbox_kind().as_deref())
        .filter(|choice| offered.offers(*choice))
        .unwrap_or(SandboxChoice::Local)
}
