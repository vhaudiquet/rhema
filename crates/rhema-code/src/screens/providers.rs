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

    /// The group the row belongs to in the add-provider dialog.
    pub fn category(self) -> ProviderCategory {
        match self {
            Self::ChatGpt | Self::Copilot | Self::OpenCodeGo | Self::Zai => {
                ProviderCategory::Subscription
            }
            Self::Google
            | Self::OpenRouter
            | Self::Anthropic
            | Self::OpenAi
            | Self::DeepSeek
            | Self::OpenCode => ProviderCategory::Api,
            Self::LlamaCpp => ProviderCategory::Local,
        }
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

    /// How many providers the category holds.
    pub fn provider_count(self) -> usize {
        providers_in(self).len()
    }
}

/// The provider rows in `category`, in display order.
pub fn providers_in(category: ProviderCategory) -> Vec<&'static ProviderEntry> {
    PROVIDERS
        .iter()
        .filter(|entry| entry.kind.category() == category)
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
}

/// The login provider list, in display order: grouped by category, the
/// subscriptions first.
pub const PROVIDERS: &[ProviderEntry] = &[
    // Subscriptions
    ProviderEntry {
        name: "ChatGPT (OpenAI)",
        kind: LoginProvider::ChatGpt,
    },
    ProviderEntry {
        name: "GitHub Copilot",
        kind: LoginProvider::Copilot,
    },
    ProviderEntry {
        name: "OpenCode Go",
        kind: LoginProvider::OpenCodeGo,
    },
    ProviderEntry {
        name: "GLM Coding Plan (Z.ai)",
        kind: LoginProvider::Zai,
    },
    // API / Pay-as-you-go
    ProviderEntry {
        name: "Google Gemini",
        kind: LoginProvider::Google,
    },
    ProviderEntry {
        name: "Anthropic",
        kind: LoginProvider::Anthropic,
    },
    ProviderEntry {
        name: "OpenAI",
        kind: LoginProvider::OpenAi,
    },
    ProviderEntry {
        name: "OpenRouter",
        kind: LoginProvider::OpenRouter,
    },
    ProviderEntry {
        name: "DeepSeek",
        kind: LoginProvider::DeepSeek,
    },
    ProviderEntry {
        name: "OpenCode (pay-as-you-go)",
        kind: LoginProvider::OpenCode,
    },
    // Local/Custom
    ProviderEntry {
        name: "llama.cpp (local server)",
        kind: LoginProvider::LlamaCpp,
    },
];

// --- Profiles ---
