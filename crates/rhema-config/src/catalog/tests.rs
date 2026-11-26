use super::*;

use crate::instances::{CLOUDFLARE_ACCOUNT_ENV, resolve_key};

#[test]
fn default_model_answers_the_registry_and_reads_unknowns_as_google() {
    assert_eq!(default_model("anthropic"), "claude-sonnet-4-5");
    assert_eq!(default_model("openrouter"), "openrouter/auto");
    assert_eq!(default_model("llama-cpp"), "local");
    assert_eq!(default_model("mistral"), "mistral-large-4-0");
    assert_eq!(default_model("mistral-vibe"), "mistral-large-4-0");
    // An unknown spelling is the unset-provider case: google's
    // default, the same fallback every caller used to hand-roll.
    assert_eq!(default_model("cohere"), "gemini-3.8-flash");
    // Every row carries one, so a new provider cannot join
    // without it.
    assert!(PROVIDERS.iter().all(|info| !info.default_model.is_empty()));
}

#[test]
fn zai_keys_read_the_two_env_names_with_zai_api_key_first() {
    let env = |name: &str| match name {
        "ZAI_API_KEY" => Some("zai-from-env".into()),
        "Z_AI_API_KEY" => Some("z-ai-from-env".into()),
        _ => None,
    };
    assert_eq!(
        resolve_key(env, provider_env_names("zai"), Some("stored".into())),
        Some("zai-from-env".into())
    );
    // The alias serves when the primary name is unset.
    let env = |name: &str| match name {
        "Z_AI_API_KEY" => Some("z-ai-from-env".into()),
        _ => None,
    };
    assert_eq!(
        resolve_key(env, provider_env_names("zai"), None),
        Some("z-ai-from-env".into())
    );
}

#[test]
fn websearch_env_names_follow_the_vendors() {
    assert_eq!(
        websearch_env_names("brave"),
        &["BRAVE_SEARCH_API_KEY", "BRAVE_API_KEY"]
    );
    assert_eq!(websearch_env_names("exa"), &["EXA_API_KEY"]);
    assert_eq!(websearch_env_names("tavily"), &["TAVILY_API_KEY"]);
    assert_eq!(websearch_env_names("opencode"), &["OPENCODE_API_KEY"]);
    let empty: &[&str] = &[];
    assert_eq!(websearch_env_names("kagi"), empty);
}

#[test]
fn decision_env_names_follow_the_vendors() {
    assert_eq!(decision_env_names("jev"), &["TYPESAFE_API_KEY"]);
    assert_eq!(
        decision_env_names("clef"),
        &["CLOUDFLARE_AUTH_TOKEN", "CLOUDFLARE_API_TOKEN"]
    );
    // The turn providers' OpenRouter spelling, the same key.
    assert_eq!(decision_env_names("openrouter"), &["OPENROUTER_API_KEY"]);
    assert_eq!(decision_env_names("opencode"), &["OPENCODE_API_KEY"]);
    let empty: &[&str] = &[];
    assert_eq!(decision_env_names("kagi"), empty);
    assert_eq!(CLOUDFLARE_ACCOUNT_ENV, "CLOUDFLARE_ACCOUNT_ID");
}
