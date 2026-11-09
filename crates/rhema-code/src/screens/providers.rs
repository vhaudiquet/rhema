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

/// The login provider list, in display order.
pub const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry {
        name: "Google Gemini",
        kind: LoginProvider::Google,
    },
    ProviderEntry {
        name: "llama.cpp (local server)",
        kind: LoginProvider::LlamaCpp,
    },
    ProviderEntry {
        name: "OpenRouter",
        kind: LoginProvider::OpenRouter,
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
        name: "DeepSeek",
        kind: LoginProvider::DeepSeek,
    },
    ProviderEntry {
        name: "Z.ai (GLM Coding Plan)",
        kind: LoginProvider::Zai,
    },
    ProviderEntry {
        name: "OpenCode (pay-as-you-go)",
        kind: LoginProvider::OpenCode,
    },
    ProviderEntry {
        name: "OpenCode Go",
        kind: LoginProvider::OpenCodeGo,
    },
    ProviderEntry {
        name: "GitHub Copilot",
        kind: LoginProvider::Copilot,
    },
];

// --- Profiles ---
