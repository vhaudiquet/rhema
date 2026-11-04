//! What the runner fixed before the app started, and the vocabulary
//! the state machine speaks: the setup, the backend a profile
//! resolves to, the screens and effects, and the config helpers the
//! agent builds read.

use super::*;

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
    /// The startup flags.
    pub options: RunOptions,
    /// The keychain fresh logins place keys in and removed logins
    /// clean up after; `None` keeps every key in the file (tests, or a
    /// runner that opted out).
    pub secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
    /// The session's assembled sandbox, when the startup choice was
    /// non-local — built and seeded before the app existed; `None` is
    /// the local fast path, the agent's own tools.
    pub sandboxed: Option<crate::sandbox::Sandboxed>,
    /// The machine half of the sandbox availability probe, probed once
    /// before the app existed: the offered set re-derives from these
    /// facts per profile — at startup, and at every profile switch.
    pub sandbox_machines: crate::sandbox::SandboxMachines,
    /// The home the app resolves machine-global paths under — the
    /// isolation seam a test points at a scratch directory so the
    /// machine's own global definitions and skills never leak into a
    /// page. `None` reads the environment (`XDG_CONFIG_HOME`, then
    /// `HOME`).
    pub home: Option<PathBuf>,
    /// Run the first-run onboarding wizard: no config existed, or the
    /// last wizard opened but never finished. `rhema onboard` sets it
    /// too, whatever the config says.
    pub onboard: bool,
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
                provider.list_models().await.map_err(|err| err.to_string())
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
/// onboarding wizard is boxed for the same reason — its dialog, model
/// rows, and skills snapshot together.
pub enum Screen {
    /// The first-run wizard: logo rail, steps, and the finish that
    /// marks the config onboarded.
    Onboarding(Box<OnboardingScreen>),
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
    /// Start one turn with this prompt: the text and the images beside
    /// it, in the order they were submitted.
    StartTurn {
        /// The prompt's text.
        text: String,
        /// The images beside the text.
        images: Vec<Image>,
    },
    /// Re-run the failed turn: no new prompt — the same request goes
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
    /// Stand the sandbox picker's choice up — container, connection,
    /// seeding — through the shared assembly. Reports back as
    /// `UiEvent::SandboxAssembled`; the session keeps its current tools
    /// until then.
    AssembleSandbox(SandboxChoice),
    /// Load this saved session off-thread for `/resume`; the parts
    /// land as `UiEvent::SessionResumed` and the app applies them.
    ResumeSession(PathBuf),
    /// Open this `SUBAGENT.md` in `$EDITOR`: the runner suspends the
    /// TUI around the blocking editor and re-enters after, and the
    /// Subagents page's rows refresh then — the edit happened behind
    /// the app's back.
    EditSubagent(PathBuf),
}

/// The Subagents page's rows: every discovered definition with its
/// provenance and the file an edit opens. The source is the file check
/// discovery shadows by — a workspace definition outranks a global one
/// of the same name, and a definition of the built-in's name replaces
/// the built-in wholesale.
pub(crate) fn subagent_rows(global: Option<&Path>, workspace: &Path) -> Vec<SubagentRow> {
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
/// discovered skill with its source — bundled skills seeded into the
/// global directory read as bundled, a workspace folder as workspace,
/// a folder under the cross-agent `~/.agents/skills` as agents, and
/// everything else global — and its on/off state from the config's
/// disabled set.
pub(crate) fn skill_rows(
    agents: Option<&Path>,
    global: Option<&Path>,
    workspace: &Path,
    disabled: &std::collections::BTreeSet<String>,
) -> Vec<SkillRow> {
    let workspace_dir = workspace.join(".rhema").join("skills");
    rhema_agent_tools::skills::discover(agents, global, workspace)
        .into_iter()
        .map(|skill| {
            let source = if skill.path.starts_with(&workspace_dir) {
                SkillSource::Workspace
            } else if agents.is_some_and(|dir| skill.path.starts_with(dir)) {
                SkillSource::Agents
            } else if crate::bundled::is_bundled(&skill.name) {
                SkillSource::Bundled
            } else {
                SkillSource::Global
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
    options: &RunOptions,
    model: Option<String>,
    disabled: &std::collections::BTreeSet<String>,
) -> CodingAgentConfig {
    CodingAgentConfig {
        max_turn_steps: options.max_steps,
        transcript: (!options.no_transcript).then(rhema_transcript::TranscriptSettings::default),
        thinking: options.thinking,
        base_system: options.base_system.clone(),
        disabled_skills: disabled.clone(),
        ..CodingAgentConfig::new(model.unwrap_or_default(), &options.workspace)
    }
}

/// The coding-agent config a session runs on: the base config, the
/// session's assembled sandbox applied when it runs on one, and the
/// subagent wire. The application order is the shared assembly's: the
/// sandbox tools and seeded skills land BEFORE the wire — the
/// subagents' inner shell inherits the executor
/// (`ShellTools::executor()`), their definition discovery stays
/// host-side — and the sandbox-side workspace path lands AFTER it.
/// The prompt sections land before the wire too: the subagents stamp
/// the config's system-prompt ingredients at wire time, so they run
/// under the same environment facts and user preferences the parent
/// sends. Returns the wire's handle beside the config: the
/// registry-backed status subscription the status strip renders one
/// live line per background subagent job from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn session_agent_config(
    options: &RunOptions,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
    model: Option<String>,
    provider: Arc<dyn ModelProvider>,
    disabled_skills: &std::collections::BTreeSet<String>,
    profile: &rhema_config::Profile,
    session_key: &str,
    disabled_mcp: &std::collections::BTreeSet<String>,
) -> (CodingAgentConfig, rhema_subagents::SubagentHandle) {
    let mut config = agent_config(options, model, disabled_skills);
    if let Some(sandboxed) = sandboxed {
        sandboxed.apply_tools(&mut config);
    }
    apply_prompt_sections(&mut config, options, sandboxed);
    // MCP: the local-only wire, gated on the sandbox — a session
    // whose bash runs elsewhere never sees the section, because its
    // executor could not reach the socket or the provisioned env.
    if let Some(entry) =
        crate::mcp_wire::wire_for_session(session_key, profile, disabled_mcp, sandboxed)
    {
        config.shell_env.extend(entry.shell_env().clone());
        config.mcp_servers = entry.lines().to_vec();
    }
    config.disabled_mcp_servers = disabled_mcp.clone();
    let (mut config, subagents) = rhema_subagents::wire(provider, config);
    if let Some(sandboxed) = sandboxed {
        sandboxed.apply_workspace(&mut config);
    }
    (config, subagents)
}

/// The prompt's two optional sections, through the one gate the
/// `system show` preview also reads — so what is previewed is what
/// turns send. The sandboxed session carries its kind's notice; a
/// local one probes the host. Runs BEFORE the subagent wire, so the
/// subagents stamp the same sections into their own prompts: the
/// sandbox-side path reads off the assembly, not the config, whose
/// workspace only becomes that path after the wire.
fn apply_prompt_sections(
    config: &mut CodingAgentConfig,
    options: &RunOptions,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
) {
    let kind = sandboxed.map(|sandboxed| sandboxed.choice.stored_name());
    let mounted = sandboxed
        .map(|sandboxed| sandboxed.workspace.clone())
        .unwrap_or_else(|| config.workspace.display().to_string());
    let (environment, user) = crate::run::prompt_sections(
        &options.workspace,
        &mounted,
        kind,
        &config.model,
        crate::run::HARNESS,
    );
    config.environment = environment;
    config.user_preferences = user;
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

/// The sandbox a session runs `bash` on when nothing was picked yet:
/// the kind the profile stores (or `RHEMA_SANDBOX` names — the same
/// env-wins layering [`crate::sandbox::resolve_choice`] reads) while
/// this machine offers it, else the kind this workspace last ran while
/// it is still offered, else `local`. The machine gates every tier: a
/// stored docker kind with no docker daemon falls through to what the
/// workspace remembers, never stands a failed container up at startup.
/// Isolation still holds: this only picks the kind up front, and the
/// assembly reads the profile itself afterwards. An explicit pick
/// never reaches here.
pub(crate) fn startup_sandbox(
    config: &rhema_config::Config,
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
/// (`RHEMA_SANDBOX` included — [`crate::sandbox::resolve_choice`]'s
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
