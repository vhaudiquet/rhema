//! The provider fleet's name-to-adapter wiring.
//!
//! Every frontend spells the same recipes, the deepseek kind is the
//! generic adapter at [`DEEPSEEK_BASE_URL`], the zai kind is the
//! Anthropic adapter at Z.ai's route under bearer auth, and each
//! spelled them in its own backend assembly, so a recipe change meant
//! three edits that had already drifted apart. This crate is the one
//! spelling: [`build`] takes the kind as rhema-config's `PROVIDERS`
//! table spells it, plus whatever the caller resolved (a key, a
//! llama.cpp server address, a stored route override), and returns
//! the turn provider and the catalog listing as trait objects. What
//! stays with the caller is policy: the retry wrapper, the default
//! model, the credential resolution itself, a registry row here is
//! wiring, not settings.
//!
//! Every listing [`build`] returns carries the model-facts decorator
//! from `rhema-model-catalog`: the adapter's rows gain the published
//! price, context, and capability facts where their own listing left
//! them blank. The adapter stays the authority on which ids exist and
//! on what it priced itself.
#![forbid(unsafe_code)]

use std::sync::Arc;

use rhema_core::ModelProvider;
use rhema_provider::ModelListing;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_provider_chatgpt::ChatGptProvider;
use rhema_provider_copilot::CopilotProvider;
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_mistral::MistralProvider;
use rhema_provider_openai_compatible::{Api, DEEPSEEK_BASE_URL, OpenAiCompatibleProvider};
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
    /// The raw turn provider. Callers wrap their own retry policy,
    /// [`rhema_provider::Retrying`] or none, on top.
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
///
/// The kinds whose endpoints speak the Responses API default to it:
/// deepseek, opencode, and openrouter always (all three are one
/// endpoint whose documented API includes `POST {base}/responses`;
/// openrouter's flip lives in its adapter, the other two here), the
/// mistral kinds always too (Mistral's reference publishes the route
/// beside chat completions, and a stored base URL still names a
/// dedicated Mistral deployment, not an alien server), and openai
/// when speaking to OpenAI itself, a stored route override
/// points at an arbitrary OpenAI-compatible server, and that one gets
/// the conservative chat completions dialect, the one every such
/// server answers. An explicit `api` (`"chat"` or `"responses"`,
/// straight from the stored instance) overrides the route rule for the
/// kinds that honor it, `openai` and the mistral pair, because a
/// user who knows their endpoint speaks Responses should not be told
/// otherwise; any other
/// spelling falls back to the rule. llama.cpp stays on chat completions too: its
/// server's `/responses` route is newer than its tool handling and its
/// default dialect is the one every build serves. ChatGPT needs no
/// choice, the Codex backend it drives is Responses-only.
pub fn build(
    name: &str,
    api_key: Option<&str>,
    llama_url: Option<&str>,
    base_url: Option<&str>,
    api: Option<&str>,
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
            let mut provider = OpenAiCompatibleProvider::new(keyed()?)
                .with_api(chosen_api(api, base_url))
                .with_dialect(openai_dialect(base_url));
            if let Some(base_url) = non_blank(base_url) {
                provider = provider.with_base_url(base_url.to_owned());
            }
            assembled(provider)
        }
        "deepseek" => {
            let mut provider = OpenAiCompatibleProvider::new(keyed()?)
                .with_base_url(DEEPSEEK_BASE_URL)
                // Their documented API surface includes `/responses`;
                // a route override still names a DeepSeek-shaped
                // endpoint.
                .with_api(Api::Responses);
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
        "mistral" | "mistral-vibe" => {
            // One wire for the API key and the Vibe subscription; the
            // subscription asks its own catalog endpoint. A stored route
            // reaches a dedicated Mistral deployment, still Mistral, so
            // the dialect follows the same rule as the home route rather
            // than dropping to the conservative chat shape an
            // OpenAI-compatible override gets.
            let mut provider = if name == "mistral-vibe" {
                MistralProvider::vibe(keyed()?)
            } else {
                MistralProvider::new(keyed()?)
            };
            if let Some(base_url) = non_blank(base_url) {
                provider = provider.with_base_url(base_url);
            }
            assembled(provider.with_api(mistral_api(api)))
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
        // build_opencode installs the facts decorator itself, because
        // the frontends also call it directly for a kept conversation.
        "opencode" => return Ok(build_opencode(&keyed()?, base_url, None)),
        _ => return Err(BuildError::Unknown(name.to_owned())),
    };
    Ok(with_facts(name, built))
}

/// Installs the model-facts decorator over the adapter's listing:
/// every catalog fetch fills the blanks the adapter left from the live
/// models.dev catalog `rhema-model-catalog` caches. The adapter still
/// decides which ids exist and what it measured itself.
fn with_facts(name: &str, mut built: Built) -> Built {
    built.listing = rhema_model_catalog::decorate_listing(name, built.listing);
    built
}

/// The OpenCode row, with the conversation id a caller keeping one
/// across rebuilds or resumes supplies. `None` lets the adapter mint a
/// fresh id, right for a new conversation, and for every frontend that
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
    // Both gateway routes serve `POST {base}/responses`; the gateway is
    // the only thing this kind addresses, so the Responses API is its
    // default dialect.
    provider = provider.with_api(Api::Responses);
    with_facts("opencode", assembled(provider))
}

/// The dialect the `openai` kind speaks: the home route is OpenAI
/// itself, so its own schema rules and the cache-routing key rides;
/// a base override names an unknown server, and the loosest schema
/// rules, nothing on the body a stricter parser would reject.
fn openai_dialect(base_url: Option<&str>) -> rhema_provider_openai_compatible::wire::Dialect {
    if non_blank(base_url).is_some() {
        rhema_provider_openai_compatible::wire::OPENAI_COMPATIBLE
    } else {
        rhema_provider_openai_compatible::wire::OPENAI
    }
}

/// A route override that is present and not whitespace, blank means
/// unset, the same reading every frontend applies to stored strings.
fn non_blank(base_url: Option<&str>) -> Option<&str> {
    base_url.map(str::trim).filter(|url| !url.is_empty())
}

/// Which dialect the mistral kinds speak. An explicit stored choice
/// (`"chat"` / `"responses"`) wins, the same override the `openai` kind
/// honors; otherwise the Responses API, home route and deployment
/// override alike, since both name Mistral, whose published surface
/// includes `POST {base}/responses`. The stored `api` field is the one
/// escape hatch an older deployment without the route needs.
fn mistral_api(api: Option<&str>) -> Api {
    match api.map(str::trim) {
        Some("chat") => Api::Chat,
        // Unset, unknown, or pinning the modern shape: the latest
        // dialect everywhere the kind addresses.
        _ => Api::Responses,
    }
}

/// Which dialect the `openai` kind speaks. An explicit stored choice
/// (`"chat"` / `"responses"`) wins; otherwise the Responses API at
/// OpenAI itself, the home route serves `POST /v1/responses`, and
/// the conservative chat-completions dialect anywhere a stored
/// override routes, since an override names an arbitrary
/// OpenAI-compatible server whose one guaranteed surface is chat
/// completions.
fn chosen_api(api: Option<&str>, base_url: Option<&str>) -> Api {
    match api.map(str::trim) {
        Some("responses") => return Api::Responses,
        Some("chat") => return Api::Chat,
        _ => {}
    }
    if non_blank(base_url).is_some() {
        Api::Chat
    } else {
        Api::Responses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_openai_kind_dialect_follows_the_route_override() {
        // No override names OpenAI itself: Responses.
        assert_eq!(chosen_api(None, None), Api::Responses);
        // A blank override reads as unset, the same reading every
        // frontend applies to stored strings.
        assert_eq!(chosen_api(None, Some("")), Api::Responses);
        assert_eq!(chosen_api(None, Some("   ")), Api::Responses);
        // A real override names an arbitrary OpenAI-compatible server:
        // chat completions, the one surface every such server answers.
        assert_eq!(
            chosen_api(None, Some("http://localhost:1234/v1")),
            Api::Chat
        );
    }

    #[test]
    fn the_mistral_kinds_default_to_the_latest_dialect() {
        // Unset, blank, or an unknown spelling: the Responses API,
        // home route and deployment override alike.
        for api in [None, Some(""), Some("graphql"), Some("responses")] {
            assert_eq!(mistral_api(api), Api::Responses, "{api:?}");
        }
        // The explicit choice that pins an older deployment back to
        // the dialect it answers.
        assert_eq!(mistral_api(Some("chat")), Api::Chat);
        // Whitespace reads as unset, the same trimming every stored
        // string gets.
        assert_eq!(mistral_api(Some("  responses  ")), Api::Responses);
    }

    #[test]
    fn an_explicit_api_choice_beats_the_route_rule() {
        // A custom endpoint the user knows serves Responses.
        assert_eq!(
            chosen_api(Some("responses"), Some("http://localhost:1234/v1")),
            Api::Responses
        );
        // And OpenAI itself can be pinned to the older shape.
        assert_eq!(chosen_api(Some("chat"), None), Api::Chat);
        // An unknown spelling falls back to the route rule.
        assert_eq!(
            chosen_api(Some("graphql"), Some("http://localhost:1234/v1")),
            Api::Chat
        );
        assert_eq!(chosen_api(Some("  responses  "), None), Api::Responses);
    }

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
            "mistral",
            "mistral-vibe",
        ] {
            let built = build(kind, Some("k"), None, None, None).expect("the kind builds");
            // One instance behind both seams: dropping either half
            // leaves the other alive, the same provider.
            let _ = built.provider;
            let _ = built.listing;
        }
    }

    #[test]
    fn a_keyed_kind_without_a_key_is_an_error_naming_the_kind() {
        assert_eq!(
            build("anthropic", None, None, None, None)
                .err()
                .expect("a keyed kind without a key fails"),
            BuildError::MissingKey("anthropic".to_owned())
        );
    }

    #[test]
    fn an_unknown_spelling_is_an_error_not_a_default() {
        assert_eq!(
            build("cohere", Some("k"), None, None, None)
                .err()
                .expect("an unknown spelling fails"),
            BuildError::Unknown("cohere".to_owned())
        );
    }

    #[test]
    fn llama_cpp_needs_a_url_and_takes_an_optional_key() {
        assert_eq!(
            build("llama-cpp", None, None, None, None)
                .err()
                .expect("llama-cpp needs a url"),
            BuildError::MissingLlamaUrl
        );
        let built = build("llama-cpp", None, Some("http://localhost:8080"), None, None)
            .expect("the url builds");
        let _ = built.provider;
        let built = build(
            "llama-cpp",
            Some("k"),
            Some("http://localhost:8080"),
            None,
            None,
        )
        .expect("the guarded server builds");
        let _ = built.provider;
    }

    #[test]
    fn route_overrides_replace_defaults_and_blanks_read_as_unset() {
        let deepseek = build("deepseek", Some("k"), None, None, None).expect("deepseek builds");
        let _ = deepseek;
        let zai = build("zai", Some("k"), None, Some("  "), None).expect("zai builds");
        let _ = zai;
        let zai = build("zai", Some("k"), None, Some("https://proxy.internal"), None)
            .expect("the override builds");
        let _ = zai;
    }
}
