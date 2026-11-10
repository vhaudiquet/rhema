//! The provider fleet's name-to-adapter wiring.
//!
//! Every frontend spells the same recipes — the deepseek kind is the
//! generic adapter at [`DEEPSEEK_BASE_URL`], the zai kind is the
//! Anthropic adapter at Z.ai's route under bearer auth — and each
//! spelled them in its own backend assembly, so a recipe change meant
//! three edits that had already drifted apart. This crate is the one
//! spelling: [`build`] takes the kind as rhema-config's `PROVIDERS`
//! table spells it, plus whatever the caller resolved (a key, a
//! llama.cpp server address, a stored route override), and returns
//! the turn provider and the catalog listing as trait objects. What
//! stays with the caller is policy: the retry wrapper, the default
//! model, the credential resolution itself — a registry row here is
//! wiring, not settings.
#![forbid(unsafe_code)]

use std::sync::Arc;

use rhema_core::ModelProvider;
use rhema_provider::ModelListing;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_provider_chatgpt::ChatGptProvider;
use rhema_provider_copilot::CopilotProvider;
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_openai_compatible::{DEEPSEEK_BASE_URL, OpenAiCompatibleProvider};
use rhema_provider_opencode::OpenCodeProvider;
use rhema_provider_openrouter::OpenRouterProvider;

/// Why a construction failed: a spelling no adapter answers to, or a
/// kind whose credential the caller never resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The kind names nothing in the fleet.
    #[error("unknown provider kind \"{0}\"")]
    Unknown(String),
    /// The kind is keyed and the caller passed no key.
    #[error("{0} needs an api key")]
    MissingKey(String),
    /// The llama.cpp kind needs a server address and none came.
    #[error("llama-cpp needs a server url")]
    MissingLlamaUrl,
}

/// One kind's adapters: what turns ride and what the catalog lists.
pub struct Built {
    /// The raw turn provider. Callers wrap their own retry policy —
    /// [`rhema_provider::Retrying`] or none — on top.
    pub provider: Arc<dyn ModelProvider>,
    /// The live model catalog. The listing stays off the retry on
    /// purpose: it answers or it fails, no ladder.
    pub listing: Arc<dyn ModelListing>,
}

/// One kind's adapters, from one concrete provider: the turn seam and
/// the catalog seam hold the same instance, coerced separately.
fn assembled<P>(provider: P) -> Built
where
    P: ModelProvider + ModelListing + 'static,
{
    let arc = Arc::new(provider);
    Built {
        provider: arc.clone(),
        listing: arc,
    }
}

/// Builds the kind's adapters. `api_key` is required for every keyed
/// kind and ignored for `llama-cpp` (an optional guard, wired when
/// present). `llama_url` is required for `llama-cpp` and ignored
/// otherwise. `base_url` is a stored route override: `None` keeps the
/// kind's default route (none for openai, deepseek's endpoint for
/// deepseek, Z.ai's plan route for zai), `Some` replaces it.
pub fn build(
    name: &str,
    api_key: Option<&str>,
    llama_url: Option<&str>,
    base_url: Option<&str>,
) -> Result<Built, BuildError> {
    let keyed = || {
        api_key
            .map(str::to_owned)
            .ok_or_else(|| BuildError::MissingKey(name.to_owned()))
    };
    let built = match name {
        "google" => assembled(GoogleProvider::new(keyed()?)),
        "anthropic" => assembled(AnthropicProvider::new(keyed()?)),
        "openrouter" => assembled(OpenRouterProvider::new(keyed()?)),
        "copilot" => assembled(CopilotProvider::new(keyed()?)),
        "chatgpt" => assembled(ChatGptProvider::new(keyed()?)),
        "openai" => {
            let mut provider = OpenAiCompatibleProvider::new(keyed()?);
            if let Some(base_url) = non_blank(base_url) {
                provider = provider.with_base_url(base_url.to_owned());
            }
            assembled(provider)
        }
        "deepseek" => {
            let mut provider =
                OpenAiCompatibleProvider::new(keyed()?).with_base_url(DEEPSEEK_BASE_URL);
            if let Some(base_url) = non_blank(base_url) {
                provider = provider.with_base_url(base_url);
            }
            assembled(provider)
        }
        "zai" => {
            let mut provider = AnthropicProvider::zai(keyed()?);
            if let Some(base_url) = non_blank(base_url) {
                provider = provider.with_base_url(base_url);
            }
            assembled(provider)
        }
        "llama-cpp" => {
            let mut provider = LlamaCppProvider::new(
                llama_url
                    .map(str::to_owned)
                    .ok_or(BuildError::MissingLlamaUrl)?,
            );
            if let Some(api_key) = api_key {
                provider = provider.with_api_key(api_key.to_owned());
            }
            assembled(provider)
        }
        "opencode" => build_opencode(&keyed()?, base_url, None),
        _ => return Err(BuildError::Unknown(name.to_owned())),
    };
    Ok(built)
}

/// The OpenCode row, with the conversation id a caller keeping one
/// across rebuilds or resumes supplies. `None` lets the adapter mint a
/// fresh id — right for a new conversation, and for every frontend that
/// hands the provider to one session only. `base_url` chooses
/// pay-as-you-go or Go.
pub fn build_opencode(api_key: &str, base_url: Option<&str>, session_id: Option<&str>) -> Built {
    let mut provider = OpenCodeProvider::new(api_key);
    if let Some(session_id) = session_id {
        provider = provider.with_session_id(session_id);
    }
    if let Some(base_url) = non_blank(base_url) {
        provider = provider.with_base_url(base_url);
    }
    assembled(provider)
}

/// A route override that is present and not whitespace — blank means
/// unset, the same reading every frontend applies to stored strings.
fn non_blank(base_url: Option<&str>) -> Option<&str> {
    base_url.map(str::trim).filter(|url| !url.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_keyed_kind_builds_with_a_key() {
        for kind in [
            "google",
            "anthropic",
            "openrouter",
            "copilot",
            "chatgpt",
            "openai",
            "deepseek",
            "zai",
            "opencode",
        ] {
            let built = build(kind, Some("k"), None, None).expect("the kind builds");
            // One instance behind both seams: dropping either half
            // leaves the other alive, the same provider.
            let _ = built.provider;
            let _ = built.listing;
        }
    }

    #[test]
    fn a_keyed_kind_without_a_key_is_an_error_naming_the_kind() {
        assert_eq!(
            build("anthropic", None, None, None)
                .err()
                .expect("a keyed kind without a key fails"),
            BuildError::MissingKey("anthropic".to_owned())
        );
    }

    #[test]
    fn an_unknown_spelling_is_an_error_not_a_default() {
        assert_eq!(
            build("mistral", Some("k"), None, None)
                .err()
                .expect("an unknown spelling fails"),
            BuildError::Unknown("mistral".to_owned())
        );
    }

    #[test]
    fn llama_cpp_needs_a_url_and_takes_an_optional_key() {
        assert_eq!(
            build("llama-cpp", None, None, None)
                .err()
                .expect("llama-cpp needs a url"),
            BuildError::MissingLlamaUrl
        );
        let built =
            build("llama-cpp", None, Some("http://localhost:8080"), None).expect("the url builds");
        let _ = built.provider;
        let built = build("llama-cpp", Some("k"), Some("http://localhost:8080"), None)
            .expect("the guarded server builds");
        let _ = built.provider;
    }

    #[test]
    fn route_overrides_replace_defaults_and_blanks_read_as_unset() {
        let deepseek = build("deepseek", Some("k"), None, None).expect("deepseek builds");
        let _ = deepseek;
        let zai = build("zai", Some("k"), None, Some("  ")).expect("zai builds");
        let _ = zai;
        let zai = build("zai", Some("k"), None, Some("https://proxy.internal"))
            .expect("the override builds");
        let _ = zai;
    }
}
