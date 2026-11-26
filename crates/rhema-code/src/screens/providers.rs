//! The login-provider catalog: the rows the pickers and dialogs offer, and what each configures.

/// Which backend a login provider row configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProvider {
    /// Google Gemini, by API key.
    Google,
    /// A llama.cpp server, by URL.
    LlamaCpp,
    /// OpenRouter, by API key.
    OpenRouter,
    /// Anthropic, by API key.
    Anthropic,
    /// OpenAI, by API key.
    OpenAi,
    /// A custom OpenAI-compatible endpoint, by base URL and key.
    OpenAiCompatible,
    /// DeepSeek, by API key (their OpenAI-compatible endpoint).
    DeepSeek,
    /// Z.ai's GLM Coding Plan, by API key (their Anthropic-compatible
    /// endpoint).
    Zai,
    /// OpenCode Console (pay-as-you-go), by API key.
    OpenCode,
    /// OpenCode Go (the subscription route), by API key.
    OpenCodeGo,
    /// GitHub Copilot, through the device-code flow.
    Copilot,
    /// A ChatGPT subscription, through the browser sign-in flow.
    ChatGpt,
    /// Mistral's hosted API, by API key.
    Mistral,
    /// A Mistral Vibe subscription, through the browser sign-in flow.
    MistralVibe,
}

impl LoginProvider {
    /// Whether the row runs an interactive flow instead of asking for a
    /// typed key.
    pub fn uses_flow(self) -> bool {
        matches!(self, Self::Copilot | Self::ChatGpt | Self::MistralVibe)
    }

    /// The group a row of this kind belongs to unless the row names
    /// another; a custom OpenAI-compatible endpoint has a row in both
    /// the API and Local/Custom groups.
    pub fn category(self) -> ProviderCategory {
        match self {
            Self::ChatGpt | Self::Copilot | Self::OpenCodeGo | Self::Zai | Self::MistralVibe => {
                ProviderCategory::Subscription
            }
            Self::Google
            | Self::OpenRouter
            | Self::Anthropic
            | Self::OpenAi
            | Self::OpenAiCompatible
            | Self::DeepSeek
            | Self::OpenCode
            | Self::Mistral => ProviderCategory::Api,
            Self::LlamaCpp => ProviderCategory::Local,
        }
    }

    /// The instance name the dialog suggests for a first login.
    pub fn default_name(self) -> &'static str {
        match self {
            Self::Google => "Google Gemini",
            Self::LlamaCpp => "llama.cpp",
            Self::OpenRouter => "OpenRouter",
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenAiCompatible => "Custom OpenAI-compatible API",
            Self::DeepSeek => "DeepSeek",
            Self::Zai => "GLM Coding Plan",
            Self::OpenCode => "OpenCode",
            Self::OpenCodeGo => "OpenCode Go",
            Self::Copilot => "GitHub Copilot",
            Self::ChatGpt => "ChatGPT",
            Self::Mistral => "Mistral API",
            Self::MistralVibe => "Mistral Vibe",
        }
    }

    /// Whether the credential stage asks for a URL before the key.
    pub fn asks_base_url(self) -> bool {
        matches!(self, Self::LlamaCpp | Self::OpenAiCompatible)
    }

    /// Whether the URL prompt must be non-empty to continue. A custom
    /// endpoint has no host of its own; llama.cpp falls back to the
    /// local default.
    pub fn base_url_required(self) -> bool {
        matches!(self, Self::OpenAiCompatible)
    }
}

/// The gateway rows whose login can offer companion providers. A
/// companion shares the login's key and writes one more instance into
/// the same profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionProvider {
    /// OpenRouter, whose key also serves its System One decision
    /// models.
    OpenRouter,
    /// OpenCode Console or Go, whose key serves decision models and
    /// Console's hosted search.
    OpenCode,
}

impl CompanionProvider {
    /// The companion a login provider's config spelling names, when it
    /// has one. Both OpenCode rows spell `opencode`.
    pub fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "openrouter" => Some(Self::OpenRouter),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }

    /// The config spelling the companion instance carries.
    pub fn config_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenCode => "opencode",
        }
    }

    /// The name the question and the notice show.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenCode => "OpenCode",
        }
    }

    /// Whether the provider serves web search beside turns and
    /// decisions. OpenRouter's search is a chat plugin, not a
    /// standalone route, so it is not offered.
    pub fn supports_websearch(self) -> bool {
        matches!(self, Self::OpenCode)
    }

    /// The cross-provider detail line for decision models.
    fn decision_detail(self) -> &'static str {
        match self {
            Self::OpenRouter => "The same key reaches Jev, Clef, and other System One models.",
            Self::OpenCode => "The same key reaches Jev and OpenCode's other System One models.",
        }
    }
}

/// One question the app asks after a gateway login: whether the same
/// key should also serve decision models or grounded web search. Each
/// yes writes one instance into the named profile, and the next
/// question, when the gateway has one, follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFollowUp {
    /// Offer the provider's System One route.
    Decision {
        /// The gateway whose key is being offered.
        provider: CompanionProvider,
        /// The profile the login landed in.
        profile: String,
        /// The provider instance whose key the new entry shares.
        instance: String,
    },
    /// Offer the provider's hosted web search.
    WebSearch {
        /// The gateway whose key is being offered.
        provider: CompanionProvider,
        /// The profile the login landed in.
        profile: String,
        /// The provider instance whose key the new entry shares.
        instance: String,
    },
}

impl ProviderFollowUp {
    /// The gateway whose key the question offers.
    pub fn provider(&self) -> CompanionProvider {
        match self {
            Self::Decision { provider, .. } | Self::WebSearch { provider, .. } => *provider,
        }
    }

    /// The line the confirm box asks.
    pub fn question(&self) -> String {
        let what = match self {
            Self::Decision { .. } => "decision models",
            Self::WebSearch { .. } => "web search",
        };
        format!(
            "Do you also want to add {what} from {}?",
            self.provider().display_name()
        )
    }

    /// The line under the question, saying what the yes costs or
    /// shares.
    pub fn detail(&self) -> String {
        match self {
            Self::Decision { provider, .. } => provider.decision_detail().to_owned(),
            Self::WebSearch { provider, .. } => match provider {
                CompanionProvider::OpenCode => {
                    "The same key reaches hosted search; each search bills $0.01.".to_owned()
                }
                CompanionProvider::OpenRouter => String::new(),
            },
        }
    }

    /// The confirm box's title.
    pub fn title(&self) -> String {
        format!(" add from {}? ", self.provider().display_name())
    }
}

/// A group of provider rows in the add-provider dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCategory {
    /// Plans billed by a recurring subscription.
    Subscription,
    /// Providers billed per token or per request.
    Api,
    /// A server on this machine or a custom endpoint.
    Local,
}

impl ProviderCategory {
    /// The categories in dialog order.
    pub const ALL: &[ProviderCategory] = &[Self::Subscription, Self::Api, Self::Local];

    /// The row's label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Subscription => "Subscriptions",
            Self::Api => "API / Pay-as-you-go",
            Self::Local => "Local/Custom",
        }
    }
}

/// The provider rows in `category`, in display order.
pub fn providers_in(category: ProviderCategory) -> Vec<&'static ProviderEntry> {
    PROVIDERS
        .iter()
        .filter(|entry| entry.category == category)
        .collect()
}

/// The provider rows shown at login. Every row configures an implemented
/// backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEntry {
    /// Display name.
    pub name: &'static str,
    /// The backend this row configures.
    pub kind: LoginProvider,
    /// The group this row appears under. Two rows may share a kind: a
    /// custom OpenAI-compatible endpoint belongs to both the API and
    /// the Local/Custom groups.
    pub category: ProviderCategory,
}

/// The login provider list, in display order: grouped by category, the
/// subscriptions first.
pub const PROVIDERS: &[ProviderEntry] = &[
    // Subscriptions
    ProviderEntry {
        name: "ChatGPT",
        kind: LoginProvider::ChatGpt,
        category: ProviderCategory::Subscription,
    },
    ProviderEntry {
        name: "GitHub Copilot",
        kind: LoginProvider::Copilot,
        category: ProviderCategory::Subscription,
    },
    ProviderEntry {
        name: "Mistral Vibe",
        kind: LoginProvider::MistralVibe,
        category: ProviderCategory::Subscription,
    },
    ProviderEntry {
        name: "OpenCode Go",
        kind: LoginProvider::OpenCodeGo,
        category: ProviderCategory::Subscription,
    },
    ProviderEntry {
        name: "GLM Coding Plan",
        kind: LoginProvider::Zai,
        category: ProviderCategory::Subscription,
    },
    // API / Pay-as-you-go
    ProviderEntry {
        name: "Google Gemini",
        kind: LoginProvider::Google,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "Anthropic",
        kind: LoginProvider::Anthropic,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "OpenAI",
        kind: LoginProvider::OpenAi,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "Custom OpenAI-compatible API",
        kind: LoginProvider::OpenAiCompatible,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "OpenRouter",
        kind: LoginProvider::OpenRouter,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "DeepSeek",
        kind: LoginProvider::DeepSeek,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "Mistral API",
        kind: LoginProvider::Mistral,
        category: ProviderCategory::Api,
    },
    ProviderEntry {
        name: "OpenCode",
        kind: LoginProvider::OpenCode,
        category: ProviderCategory::Api,
    },
    // Local/Custom
    ProviderEntry {
        name: "llama.cpp",
        kind: LoginProvider::LlamaCpp,
        category: ProviderCategory::Local,
    },
    ProviderEntry {
        name: "Custom OpenAI-compatible API",
        kind: LoginProvider::OpenAiCompatible,
        category: ProviderCategory::Local,
    },
];

// --- Profiles ---
