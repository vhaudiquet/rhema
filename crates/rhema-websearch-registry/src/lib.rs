//! The web search fleet's name-to-adapter wiring.
//!
//! Every frontend spells the same recipes, the `brave` kind is the
//! Brave adapter with its `X-Subscription-Token` header, the `tavily`
//! kind is the Tavily adapter under bearer auth, and without this
//! crate each would spell them in its own assembly. This is the one
//! spelling: [`build`] takes the kind as the config spells it, plus
//! the key the caller resolved, and returns the provider as a trait
//! object. What stays with the caller is policy: the chain, the
//! order, the credential resolution itself, a registry call here is
//! wiring, not settings (`specs/rhema-websearch.md` §8).
#![forbid(unsafe_code)]

use std::sync::Arc;

use rhema_config::secrets::SecretStore;
use rhema_config::{Profile, websearch_env_names};
#[cfg(feature = "mock")]
use rhema_websearch::MockSearchProvider;
use rhema_websearch::{Chain, WebSearchProvider};
use rhema_websearch_brave::BraveProvider;
use rhema_websearch_exa::ExaProvider;
use rhema_websearch_tavily::TavilyProvider;

/// Why a construction failed: a spelling no adapter answers to, or a
/// kind whose key the caller never resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The kind names nothing in the fleet.
    #[error("unknown websearch provider kind \"{0}\"")]
    Unknown(String),
    /// Every kind is keyed and the caller passed no key.
    #[error("{0} needs an api key")]
    MissingKey(String),
}

/// The vendor spellings the config uses, in the env-only tier's fixed
/// order (`specs/rhema-websearch.md` §3).
pub const KINDS: [&str; 3] = ["brave", "exa", "tavily"];

/// Builds the kind's provider from the key the caller resolved. The
/// constructors are the adapters' defaults: no route overrides, no
/// vendor knobs, the settings have no fields for them in v1.
///
/// # Errors
///
/// [`BuildError::Unknown`] when `kind` names nothing in
/// [`KINDS`], [`BuildError::MissingKey`] when `api_key` is none:
/// every kind here is keyed.
pub fn build(
    kind: &str,
    api_key: Option<String>,
) -> Result<Arc<dyn WebSearchProvider>, BuildError> {
    if !KINDS.contains(&kind) {
        return Err(BuildError::Unknown(kind.to_owned()));
    }
    let Some(api_key) = api_key else {
        return Err(BuildError::MissingKey(kind.to_owned()));
    };
    match kind {
        "brave" => Ok(Arc::new(BraveProvider::new(api_key))),
        "exa" => Ok(Arc::new(ExaProvider::new(api_key))),
        // The spelling is validated above; the arm is exhaustive.
        _ => Ok(Arc::new(TavilyProvider::new(api_key))),
    }
}

/// Builds the test provider the registry's own consumers' offline
/// tests drive: the base crate's [`MockSearchProvider`] behind the
/// same trait object `build` returns, under any kind spelling.
/// Feature `mock`, a test seam: production never calls it.
#[cfg(feature = "mock")]
pub fn build_mock(kind: &'static str) -> Arc<dyn WebSearchProvider> {
    Arc::new(MockSearchProvider::new(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_builds_from_a_key() {
        for kind in KINDS {
            let provider = build(kind, Some("key".into())).unwrap();
            assert_eq!(provider.kind(), kind);
        }
    }

    #[test]
    fn a_missing_key_is_a_missing_key_not_an_unknown() {
        assert!(
            matches!(build("brave", None), Err(BuildError::MissingKey(kind)) if kind == "brave")
        );
    }

    #[test]
    fn an_unknown_spelling_names_itself() {
        assert!(
            matches!(build("kagi", Some("key".into())), Err(BuildError::Unknown(kind)) if kind == "kagi")
        );
        assert!(
            matches!(build("kagi", None), Err(BuildError::Unknown(kind)) if kind == "kagi"),
            "an unknown kind is unknown before it is keyed"
        );
    }
}

/// The profile's chain, assembled: the configured instances in
/// [`Profile::websearch_order`], each key resolved through the tiers
/// ([`WebSearchInstance::api_key`](rhema_config::WebSearchInstance::api_key)), instances whose key stays
/// unresolved or whose kind spells nothing in [`KINDS`] skipped, and
/// over an empty result the env-only tier: one entry per kind whose
/// environment variable resolves, in the fixed kind order. `None`
/// when neither tier yields: no providers, no function, no prompt
/// section (`specs/rhema-websearch.md` §8).
///
/// The budgets are the base crate's defaults; per-attempt settings
/// are v1's deliberate absence. Cheap per call, one map walk and a
/// few constructor clones, so a mode handler assembles per invocation
/// and never caches settings.
pub fn assemble_chain(
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Option<Arc<Chain>> {
    assemble_chain_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// The chain's entries as `(name, kind, key)`: what
/// [`assemble_chain`] would build, without building the HTTP clients.
/// A frontend that hands the chain to another process serializes this
/// instead of the providers.
pub fn selections(
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Vec<(String, String, String)> {
    selections_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// [`assemble_chain`] over a caller-supplied environment, the seam
/// tests drive instead of mutating the process environment.
pub fn assemble_chain_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Option<Arc<Chain>> {
    let mut chain = Chain::new();
    for (name, kind, key) in selections_with(env, profile, secrets) {
        let Ok(provider) = build(&kind, Some(key)) else {
            continue;
        };
        chain.push(name, provider);
    }
    if chain.is_empty() {
        None
    } else {
        Some(Arc::new(chain))
    }
}

/// Whether [`assemble_chain`] would return a chain, without building a
/// provider: the same tier walk, the same key resolution, the same
/// gate. A preview that must not construct HTTP clients asks this, and
/// a frontend deciding whether to render the prompt's section asks the
/// same question the wire does.
pub fn would_wire(profile: Option<&Profile>, secrets: Option<&dyn SecretStore>) -> bool {
    would_wire_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// [`would_wire`] over a caller-supplied environment, the seam tests
/// drive instead of mutating the process environment.
pub fn would_wire_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> bool {
    !selections_with(env, profile, secrets).is_empty()
}

/// The providers the chain would carry, as name, kind, and key: the
/// profile's instances in [`Profile::websearch_order`] whose key
/// resolves and whose kind spells something in [`KINDS`], and over
/// none of those the env-only tier, one entry per kind whose variable
/// resolves, in the fixed kind order.
fn selections_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Vec<(String, String, String)> {
    let mut selections = Vec::new();
    if let Some(profile) = profile {
        for (name, instance) in profile.websearch_chain() {
            if !KINDS.contains(&instance.provider.as_str()) {
                continue;
            }
            let Some(key) = instance.api_key(secrets) else {
                continue;
            };
            selections.push((name, instance.provider.clone(), key));
        }
    }
    if selections.is_empty() {
        for kind in KINDS {
            let key = websearch_env_names(kind)
                .iter()
                .filter_map(|name| env(name))
                .map(|value| value.trim().to_owned())
                .find(|value| !value.is_empty());
            if let Some(key) = key {
                selections.push((kind.to_owned(), kind.to_owned(), key));
            }
        }
    }
    selections
}

#[cfg(test)]
mod assembly_tests {
    use super::*;
    use rhema_config::WebSearchInstance;
    use rhema_config::secrets::VolatileSecretStore;

    fn instance(kind: &str, key: Option<&str>) -> WebSearchInstance {
        WebSearchInstance {
            provider: kind.into(),
            api_key: key.map(str::to_owned),
            secret_ref: None,
        }
    }

    #[test]
    fn the_chain_walks_the_order_with_resolved_keys() {
        let mut profile = Profile::default();
        profile
            .websearch
            .insert("exa-work".into(), instance("exa", Some("k1")));
        profile
            .websearch
            .insert("brave-main".into(), instance("brave", Some("k2")));
        profile.websearch_order = vec!["brave-main".into(), "exa-work".into()];
        let chain = assemble_chain_with(|_| None, Some(&profile), None).unwrap();
        // The order, not the map's iteration order.
        assert_eq!(chain.names(), vec!["brave-main", "exa-work"]);
    }

    #[test]
    fn keys_resolve_through_the_keychain_tier() {
        let store = VolatileSecretStore::new();
        store.set("ws-1", "chain-key").unwrap();
        let mut profile = Profile::default();
        let mut brave = instance("brave", None);
        brave.secret_ref = Some("ws-1".into());
        profile.websearch.insert("brave-main".into(), brave);
        profile.websearch_order = vec!["brave-main".into()];
        // An unreachable-looking chain would fail the run; the built
        // provider's key lives in the store, the entry survives. The
        // proof is the chain's existence: an unresolved key drops the
        // entry (the test above), this one holds.
        let chain = assemble_chain_with(|_| None, Some(&profile), Some(&store)).unwrap();
        assert_eq!(chain.names(), vec!["brave-main"]);
    }

    /// The preview's question, the wire's answer: `would_wire` walks the
    /// same tiers as `assemble_chain` and agrees with it on every case
    /// the tests above cover.
    #[test]
    fn would_wire_answers_what_assemble_chain_returns() {
        let store = VolatileSecretStore::new();
        store.set("ws-1", "chain-key").unwrap();
        let keyed = {
            let mut profile = Profile::default();
            let mut brave = instance("brave", None);
            brave.secret_ref = Some("ws-1".into());
            profile.websearch.insert("brave-main".into(), brave);
            profile.websearch_order = vec!["brave-main".into()];
            profile
        };
        let keyless = {
            let mut profile = Profile::default();
            profile
                .websearch
                .insert("brave-main".into(), instance("brave", None));
            profile.websearch_order = vec!["brave-main".into()];
            profile
        };
        assert!(would_wire_with(|_| None, Some(&keyed), Some(&store)));
        assert!(assemble_chain_with(|_| None, Some(&keyed), Some(&store)).is_some());
        assert!(!would_wire_with(|_| None, Some(&keyless), None));
        assert!(assemble_chain_with(|_| None, Some(&keyless), None).is_none());
        let env = |name: &str| (name == "EXA_API_KEY").then(|| "k".to_owned());
        assert!(would_wire_with(env, None, None));
        assert!(assemble_chain_with(env, None, None).is_some());
        assert!(!would_wire_with(|_| None, None, None));
    }

    #[test]
    fn an_unresolved_key_skips_the_entry() {
        let mut profile = Profile::default();
        profile
            .websearch
            .insert("brave-main".into(), instance("brave", None));
        profile.websearch_order = vec!["brave-main".into()];
        assert!(
            assemble_chain_with(|_| None, Some(&profile), None).is_none(),
            "one instance, no key anywhere: no chain"
        );
    }

    #[test]
    fn an_unknown_kind_skips_the_entry_without_killing_the_chain() {
        let mut profile = Profile::default();
        profile
            .websearch
            .insert("weird".into(), instance("kagi", Some("k")));
        profile
            .websearch
            .insert("exa-work".into(), instance("exa", Some("k")));
        profile.websearch_order = vec!["weird".into(), "exa-work".into()];
        let chain = assemble_chain_with(|_| None, Some(&profile), None).unwrap();
        assert_eq!(chain.names(), vec!["exa-work"]);
    }

    #[test]
    fn the_env_only_tier_serves_a_profile_with_no_section() {
        let env = |name: &str| (name == "EXA_API_KEY").then(|| " from-env ".to_owned());
        let chain = assemble_chain_with(env, None, None).unwrap();
        assert_eq!(chain.names(), vec!["exa"]);
    }

    #[test]
    fn the_env_only_tier_walks_the_fixed_kind_order() {
        let env = |name: &str| match name {
            "TAVILY_API_KEY" => Some("t".to_owned()),
            "BRAVE_SEARCH_API_KEY" => Some("b".to_owned()),
            _ => None,
        };
        let chain = assemble_chain_with(env, None, None).unwrap();
        assert_eq!(chain.names(), vec!["brave", "tavily"]);
    }

    #[test]
    fn configured_instances_win_over_the_env_only_tier() {
        let mut profile = Profile::default();
        profile
            .websearch
            .insert("brave-main".into(), instance("brave", Some("stored")));
        profile.websearch_order = vec!["brave-main".into()];
        let env = |name: &str| (name == "EXA_API_KEY").then(|| "k".to_owned());
        let chain = assemble_chain_with(env, Some(&profile), None).unwrap();
        assert_eq!(
            chain.names(),
            vec!["brave-main"],
            "the env tier does not append to a chain"
        );
    }

    #[test]
    fn neither_tier_yields_no_chain() {
        assert!(assemble_chain_with(|_| None, None, None).is_none());
        assert!(assemble_chain_with(|_| None, Some(&Profile::default()), None).is_none());
    }
}
