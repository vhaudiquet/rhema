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
}

impl LoginProvider {
    /// Whether the row runs an interactive flow instead of asking for a
    /// typed key.
    pub fn uses_flow(self) -> bool {
        matches!(self, Self::Copilot | Self::ChatGpt)
    }

    /// The group a row of this kind belongs to unless the row names
    /// another; a custom OpenAI-compatible endpoint has a row in both
    /// the API and Local/Custom groups.
    pub fn category(self) -> ProviderCategory {
        match self {
            Self::ChatGpt | Self::Copilot | Self::OpenCodeGo | Self::Zai => {
                ProviderCategory::Subscription
            }
            Self::Google
            | Self::OpenRouter
            | Self::Anthropic
            | Self::OpenAi
            | Self::OpenAiCompatible
            | Self::DeepSeek
            | Self::OpenCode => ProviderCategory::Api,
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
