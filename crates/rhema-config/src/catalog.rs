//! The provider catalog: the config spellings, the credential
//! environment names, and the fallback models, one row per adapter.

/// Where [`crate::profile::Profile::place_key`] landed a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLocation {
    /// The encrypted secret store: the instance carries only the
    /// entry's `secret_ref`.
    Store,
    /// The config file itself: the store refused, so the literal
    /// stayed, the shape the file has always held.
    File,
}

/// The environment variables whose values override a stored key for
/// `provider`, in precedence order.
/// One provider's config-facing facts, one row per adapter: the
/// spelling the config file and `RHEMA_PROVIDER` use, the environment
/// variables a credential resolves through, and the fix to name when
/// none does. Adding a provider means adding a row here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// The config spelling: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, `deepseek`, `zai`, `opencode`, `copilot`,
    /// `chatgpt`, `mistral`, `mistral-vibe`.
    pub name: &'static str,
    /// The environment variables a credential resolves through, in
    /// priority order.
    pub env_keys: &'static [&'static str],
    /// Whether the provider has a login flow: a credential the user
    /// enters or a device grant answers for. `false` for llama.cpp,
    /// a server is an address to point at, not a subscription.
    pub login: bool,
    /// The fix a frontend without a login screen names when no
    /// credential resolves for this provider.
    pub missing_key_help: &'static str,
    /// The model the provider runs when neither the user nor the
    /// frontend names one: an id the adapter keeps answering for,
    /// chosen once here so every frontend falls back the same way.
    /// A safety net, not the normal path, the login and picker
    /// flows store a model.
    pub default_model: &'static str,
}

/// The provider registry, in the order the pickers list them. A
/// provider joins with all of itself: spelling, credential rule,
/// default model, the facts every frontend needs, written once.
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "google",
        env_keys: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set GEMINI_API_KEY (or log in once interactively)",
        default_model: "gemini-3.8-flash",
    },
    ProviderInfo {
        name: "llama-cpp",
        env_keys: &["LLAMA_CPP_API_KEY"],
        login: false,
        missing_key_help: "missing API key: set LLAMA_CPP_API_KEY",
        default_model: "local",
    },
    ProviderInfo {
        name: "openrouter",
        env_keys: &["OPENROUTER_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set OPENROUTER_API_KEY",
        default_model: "openrouter/auto",
    },
    ProviderInfo {
        name: "anthropic",
        env_keys: &["ANTHROPIC_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set ANTHROPIC_API_KEY (or log in once interactively)",
        default_model: "claude-sonnet-4-5",
    },
    ProviderInfo {
        name: "openai",
        env_keys: &["OPENAI_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set OPENAI_API_KEY (or log in once interactively)",
        default_model: "gpt-5",
    },
    ProviderInfo {
        name: "chatgpt",
        env_keys: &["CHATGPT_ACCESS_TOKEN"],
        login: true,
        missing_key_help: "missing credential: sign in with ChatGPT (or set CHATGPT_ACCESS_TOKEN)",
        default_model: "gpt-5.1-codex",
    },
    ProviderInfo {
        name: "mistral",
        env_keys: &["MISTRAL_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set MISTRAL_API_KEY (or log in once interactively)",
        default_model: "mistral-large-4-0",
    },
    ProviderInfo {
        name: "mistral-vibe",
        env_keys: &["MISTRAL_VIBE_API_KEY"],
        login: true,
        missing_key_help: "missing credential: sign in with Mistral Vibe (or set MISTRAL_VIBE_API_KEY)",
        default_model: "mistral-large-4-0",
    },
    ProviderInfo {
        name: "deepseek",
        env_keys: &["DEEPSEEK_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set DEEPSEEK_API_KEY (or log in once interactively)",
        default_model: "deepseek-chat",
    },
    ProviderInfo {
        name: "zai",
        env_keys: &["ZAI_API_KEY", "Z_AI_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set ZAI_API_KEY (or log in once interactively)",
        default_model: "GLM-5.3",
    },
    ProviderInfo {
        name: "opencode",
        env_keys: &["OPENCODE_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set OPENCODE_API_KEY (or log in once interactively)",
        default_model: "deepseek-v4.1-flash",
    },
    ProviderInfo {
        name: "copilot",
        env_keys: &["GITHUB_COPILOT_TOKEN"],
        login: true,
        missing_key_help: "missing API key: set GITHUB_COPILOT_TOKEN (or log in once interactively)",
        default_model: "gpt-4.1",
    },
];

/// The model a provider runs when nothing stored names one, the
/// registry row's [`ProviderInfo::default_model`]. A name no row
/// answers to reads as google's, the same default an unset provider
/// gets; callers that reject unknown spellings do so before asking.
pub fn default_model(name: &str) -> &'static str {
    provider_info(name).unwrap_or(&PROVIDERS[0]).default_model
}

/// Looks a provider up by config spelling; `None` for anything no row
/// answers to.
pub fn provider_info(name: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|info| info.name == name)
}

pub(crate) fn provider_env_names(provider: &str) -> &'static [&'static str] {
    provider_info(provider)
        .map(|info| info.env_keys)
        .unwrap_or(&[])
}

/// The environment variable names a web search kind's key rides: the
/// vendors' own spellings, Brave's documented name first, the common
/// shorthand second (`specs/rhema-websearch.md` §12.9).
pub fn websearch_env_names(kind: &str) -> &'static [&'static str] {
    match kind {
        "brave" => &["BRAVE_SEARCH_API_KEY", "BRAVE_API_KEY"],
        "exa" => &["EXA_API_KEY"],
        "tavily" => &["TAVILY_API_KEY"],
        // The turn provider's key: one Console key serves Go and
        // hosted search in the workspace it belongs to.
        "opencode" => &["OPENCODE_API_KEY"],
        _ => &[],
    }
}

/// The environment variable names a decision kind's key rides: the
/// vendors' own spellings, the documented name first, the common one
/// second (`specs/rhema-decision.md` §3). Clef's account id rides
/// [`crate::instances::CLOUDFLARE_ACCOUNT_ENV`] beside them.
pub fn decision_env_names(kind: &str) -> &'static [&'static str] {
    match kind {
        "jev" => &["TYPESAFE_API_KEY"],
        "clef" => &["CLOUDFLARE_AUTH_TOKEN", "CLOUDFLARE_API_TOKEN"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        // The turn provider's key: OpenCode Go covers the decision
        // models beside the text ones.
        "opencode" => &["OPENCODE_API_KEY"],
        _ => &[],
    }
}
#[cfg(test)]
mod tests;
