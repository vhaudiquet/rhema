//! The decision fleet's name-to-adapter wiring.
//!
//! Every kind speaks the System One wire, so without this crate a
//! frontend would still spell each one's constructor: the `jev` kind
//! is the TypeSafe adapter with its model field, the `clef` kind is
//! the Workers AI adapter, which also needs the account id its route
//! runs on, the `openrouter` kind is the aggregator's Decisions
//! route, one key to Jev, Clef and forty-odd others by slug. This is
//! the one spelling: [`build`] takes the instance as
//! the config spells it, plus the key the caller resolved, and
//! returns the provider as a trait object; [`resolve`] is the
//! frontend's whole resolution, the profile's named or default
//! instance, else, for a profile holding none, the environment tier.
//! What stays with the caller is
//! the credential resolution itself: a registry call here is wiring,
//! not settings (`specs/rhema-decision.md` §8, the
//! `rhema-websearch-registry` arrangement).
#![forbid(unsafe_code)]

use std::sync::Arc;

use rhema_config::secrets::SecretStore;
use rhema_config::{CLOUDFLARE_ACCOUNT_ENV, DecisionInstance, Profile, decision_env_names};
use rhema_decision::DecisionProvider;
use rhema_decision_clef::{ClefModel, ClefProvider};
use rhema_decision_jev::JevProvider;
use rhema_decision_openrouter::OpenRouterProvider;

/// Why a construction failed: a spelling no adapter answers to, a
/// kind whose key the caller never resolved, a Clef instance with no
/// account to run on, or a model spelling the kind does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The kind names nothing in the fleet.
    #[error("unknown decision provider kind \"{0}\"")]
    Unknown(String),
    /// Every kind is keyed and the caller passed no key.
    #[error("{0} needs an api key")]
    MissingKey(String),
    /// Clef runs on an account, and none was resolved.
    #[error("clef needs an account id ({CLOUDFLARE_ACCOUNT_ENV} or the instance's account_id)")]
    MissingAccountId,
    /// The model spelling is not one the kind serves.
    #[error("{kind} has no model \"{model}\"")]
    UnknownModel {
        /// The kind the model was asked of.
        kind: String,
        /// The spelling the kind does not have.
        model: String,
    },
}

/// The vendor spellings the config uses, in the environment tier's
/// fixed order: the first kind whose variables resolve is the one
/// asked (`specs/rhema-decision.md` §3). The vendors' own keys come
/// before the aggregator's: a machine holding several is asked the
/// most direct one.
pub const KINDS: [&str; 3] = ["jev", "clef", "openrouter"];

/// Builds the instance's provider from the key the caller resolved.
/// The model is the instance's: Jev defaults to its moving alias,
/// Clef to the 27B model, OpenRouter to Jev 1.14 by slug; the account
/// id is resolved here too, since only Clef needs one and an instance
/// of another kind without it must not fail.
///
/// # Errors
///
/// [`BuildError::Unknown`] when the kind names nothing in [`KINDS`],
/// [`BuildError::MissingKey`] on a blank key,
/// [`BuildError::MissingAccountId`] for a Clef instance with no
/// account, [`BuildError::UnknownModel`] for a model the kind lacks.
pub fn build(
    instance: &DecisionInstance,
    key: String,
) -> Result<Arc<dyn DecisionProvider>, BuildError> {
    let kind = instance.provider.as_str();
    if !KINDS.contains(&kind) {
        return Err(BuildError::Unknown(kind.to_owned()));
    }
    if key.trim().is_empty() {
        return Err(BuildError::MissingKey(kind.to_owned()));
    }
    let model = instance
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let base_url = instance
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty());
    match kind {
        "jev" => {
            let provider = JevProvider::new(key);
            let provider = match model {
                Some(model) => provider.with_model(model),
                None => provider,
            };
            let provider = match base_url {
                Some(base_url) => provider.with_base_url(base_url),
                None => provider,
            };
            Ok(Arc::new(provider))
        }
        "openrouter" => {
            // The model is a slug, free text like Jev's: the catalog
            // moves faster than any list compiled here could.
            let provider = OpenRouterProvider::new(key);
            let provider = match model {
                Some(model) => provider.with_model(model),
                None => provider,
            };
            let provider = match base_url {
                Some(base_url) => provider.with_base_url(base_url),
                None => provider,
            };
            Ok(Arc::new(provider))
        }
        // The spelling is validated above; the one left is Clef.
        _ => {
            let account_id = instance.account_id().ok_or(BuildError::MissingAccountId)?;
            let model = match model {
                Some(spelling) => {
                    ClefModel::parse(spelling).ok_or_else(|| BuildError::UnknownModel {
                        kind: kind.to_owned(),
                        model: spelling.to_owned(),
                    })?
                }
                None => ClefModel::default(),
            };
            let provider = ClefProvider::new(account_id, key).with_model(model);
            let provider = match base_url {
                Some(base_url) => provider.with_base_url(base_url),
                None => provider,
            };
            Ok(Arc::new(provider))
        }
    }
}

/// The model an instance of `kind` asks when its `model` is unset or
/// blank: the spelling [`build`] defaults to, so a settings page can
/// show what a blank means without a copy of the adapters' constants.
/// `None` for a kind nothing answers to.
pub fn default_model(kind: &str) -> Option<&'static str> {
    match kind {
        "jev" => Some(rhema_decision_jev::DEFAULT_MODEL),
        "openrouter" => Some(rhema_decision_openrouter::DEFAULT_MODEL),
        "clef" => Some(ClefModel::default().as_str()),
        _ => None,
    }
}

/// Builds the test provider the registry's own consumers' offline
/// tests drive: the base crate's `MockDecisionProvider` behind the
/// same trait object `build` returns, under any kind spelling.
/// Feature `mock`, a test seam: production never calls it.
#[cfg(feature = "mock")]
pub fn build_mock(kind: &'static str) -> Arc<dyn DecisionProvider> {
    use rhema_decision::mock::MockDecisionProvider;
    Arc::new(MockDecisionProvider::new(kind))
}

/// Why a profile yields no decision provider. Every arm is the
/// config's to fix, and the text names the entry or the variable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The profile holds no decision instance and the environment
    /// serves no kind.
    #[error(
        "no decision instance is configured (a `decision` section with an \
         `instances` entry in the profile, or a TYPESAFE_API_KEY, \
         CLOUDFLARE_AUTH_TOKEN + CLOUDFLARE_ACCOUNT_ID or OPENROUTER_API_KEY \
         environment variable)"
    )]
    Unconfigured,
    /// The profile's default instance, not buildable: a kind
    /// nothing answers to, a key resolving through no tier, an
    /// account Clef lacks, a model the kind does not have.
    #[error("decision instance \"{name}\": {source}")]
    Build {
        /// The profile's name for the instance.
        name: String,
        /// What stopped the build.
        #[source]
        source: BuildError,
    },
}

/// The profile's decision provider: the default instance
/// ([`Profile::active_decision_instance`]: the one the `decision`
/// section's `instance` names, else its first), built on the key that
/// instance's kind resolves through the tiers
/// ([`DecisionInstance::api_key`]). Never another instance: an entry
/// that does not build is [`ResolveError::Build`], for the model to
/// read. A profile holding no instance falls to the environment tier:
/// the first kind in [`KINDS`] whose variable resolves (Clef also
/// needs [`CLOUDFLARE_ACCOUNT_ENV`]); neither yielding is
/// [`ResolveError::Unconfigured`] (`specs/rhema-decision.md` §3).
///
/// Cheap per call, one keychain read and a few constructor clones, so
/// a mode handler resolves per invocation and never caches settings.
pub fn resolve(
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Result<Arc<dyn DecisionProvider>, ResolveError> {
    resolve_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// The instance [`resolve`] would build, plus the key it would use,
/// without building the provider: what a frontend hands to another
/// process. A profile instance with no key still returns, with `None`,
/// so the call reports the missing key where it runs instead of the
/// macro vanishing.
pub fn selection(
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Option<(DecisionInstance, Option<String>)> {
    selection_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// [`selection`] over a caller-supplied environment.
fn selection_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Option<(DecisionInstance, Option<String>)> {
    if let Some((_, instance)) = profile.and_then(Profile::active_decision_instance) {
        let key = instance.api_key(secrets);
        return Some((instance.clone(), key));
    }
    environment_tier(env).map(|(instance, key)| (instance, Some(key)))
}

/// [`resolve`] over a caller-supplied environment, the seam tests
/// drive instead of mutating the process environment.
pub fn resolve_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> Result<Arc<dyn DecisionProvider>, ResolveError> {
    let Some((name, instance)) = profile.and_then(Profile::active_decision_instance) else {
        let (instance, key) = environment_tier(env).ok_or(ResolveError::Unconfigured)?;
        return build(&instance, key).map_err(|source| ResolveError::Build {
            name: instance.provider,
            source,
        });
    };
    // The kind first: an unknown kind with no key is unknown, not
    // keyless.
    if !KINDS.contains(&instance.provider.as_str()) {
        return Err(ResolveError::Build {
            name: name.to_owned(),
            source: BuildError::Unknown(instance.provider.clone()),
        });
    }
    let key = instance
        .api_key(secrets)
        .ok_or_else(|| ResolveError::Build {
            name: name.to_owned(),
            source: BuildError::MissingKey(instance.provider.clone()),
        })?;
    build(instance, key).map_err(|source| ResolveError::Build {
        name: name.to_owned(),
        source,
    })
}

/// Whether the session wires the macro for this profile. A profile
/// holding a decision instance wires even when the instance does not
/// build, so the call reports the entry's problem instead of the
/// function vanishing; a profile holding none wires when the
/// environment tier serves. Without building a provider.
pub fn is_configured(profile: Option<&Profile>, secrets: Option<&dyn SecretStore>) -> bool {
    is_configured_with(|name| std::env::var(name).ok(), profile, secrets)
}

/// [`is_configured`] over a caller-supplied environment, the seam
/// tests drive instead of mutating the process environment.
pub fn is_configured_with(
    env: impl Fn(&str) -> Option<String>,
    profile: Option<&Profile>,
    secrets: Option<&dyn SecretStore>,
) -> bool {
    profile
        .and_then(Profile::active_decision_instance)
        .is_some()
        || resolve_with(env, profile, secrets).is_ok()
}

/// The environment tier: one candidate per kind in [`KINDS`], the
/// first whose key variable resolves (Clef needs the account variable
/// too, since there is no file to read one from).
fn environment_tier(env: impl Fn(&str) -> Option<String>) -> Option<(DecisionInstance, String)> {
    for kind in KINDS {
        let key = decision_env_names(kind)
            .iter()
            .filter_map(|name| env(name))
            .map(|value| value.trim().to_owned())
            .find(|value| !value.is_empty());
        let Some(key) = key else {
            continue;
        };
        let account_id = env(CLOUDFLARE_ACCOUNT_ENV)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if kind == "clef" && account_id.is_none() {
            continue;
        }
        return Some((
            DecisionInstance {
                provider: kind.to_owned(),
                api_key: None,
                secret_ref: None,
                model: None,
                account_id,
                base_url: None,
            },
            key,
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_config::DecisionSettings;
    use rhema_config::secrets::VolatileSecretStore;

    fn instance(kind: &str, key: Option<&str>) -> DecisionInstance {
        DecisionInstance {
            provider: kind.into(),
            api_key: key.map(str::to_owned),
            secret_ref: None,
            model: None,
            account_id: Some("acc".into()),
            base_url: None,
        }
    }

    #[tokio::test]
    async fn a_base_url_routes_the_call_to_a_self_hosted_server() {
        // A System One server on localhost: the jev kind against a
        // wiremock standing in for Ollaya or vLLM.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "local", "answers": {"q": {"type": "noul", "noul": 0.5}},
                "usage": {"input_tokens": 1, "output_tokens": 0}
            })))
            .mount(&server)
            .await;
        let mut routed = instance("jev", Some("k"));
        routed.base_url = Some(server.uri());
        let provider = build(&routed, "k".into()).unwrap();
        let request = rhema_decision::DecisionRequest::new("s")
            .with_question("q", rhema_decision::Question::noul("x?"));
        assert_eq!(provider.decide(&request).await.unwrap().model, "local");
    }

    #[tokio::test]
    async fn the_openrouter_kind_rides_the_decisions_route() {
        // The router's route, on a wiremock standing in for it.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/alpha/decisions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "typesafe/jev-1.14", "provider": "TypeSafe",
                "answers": {"q": {"type": "noul", "noul": 0.5, "confidence": 0.5}},
                "usage": {"input_tokens": 1, "output_tokens": 0, "cost": 0.0}
            })))
            .mount(&server)
            .await;
        let mut routed = instance("openrouter", Some("k"));
        routed.base_url = Some(server.uri());
        routed.model = Some("cloudflare/clef-flash".into());
        let provider = build(&routed, "k".into()).unwrap();
        let request = rhema_decision::DecisionRequest::new("s")
            .with_question("q", rhema_decision::Question::noul("x?"));
        assert_eq!(
            provider.decide(&request).await.unwrap().model,
            "typesafe/jev-1.14"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap().remove(0).body)
                .unwrap();
        assert_eq!(body["model"], "cloudflare/clef-flash");
    }

    #[test]
    fn every_kind_builds_from_its_key() {
        for kind in KINDS {
            let provider = build(&instance(kind, Some("key")), "key".into()).unwrap();
            assert_eq!(provider.kind(), kind);
        }
    }

    #[test]
    fn the_errors_name_their_cause() {
        assert!(matches!(
            build(&instance("kagi", Some("k")), "k".into()),
            Err(BuildError::Unknown(kind)) if kind == "kagi"
        ));
        assert!(
            matches!(build(&instance("jev", None), " ".into()), Err(BuildError::MissingKey(kind)) if kind == "jev")
        );
        let mut clef = instance("clef", Some("k"));
        clef.account_id = None;
        assert_eq!(
            build(&clef, "k".into()).err().unwrap(),
            BuildError::MissingAccountId
        );
        let mut wrong = instance("clef", Some("k"));
        wrong.model = Some("jev".into());
        assert_eq!(
            build(&wrong, "k".into()).err().unwrap(),
            BuildError::UnknownModel {
                kind: "clef".into(),
                model: "jev".into()
            }
        );
        // A Jev instance needs no account: the model is free text.
        let mut jev = instance("jev", Some("k"));
        jev.account_id = None;
        jev.model = Some("jev-preview".into());
        assert!(build(&jev, "k".into()).is_ok());
        // Nor does an OpenRouter one: the model is a slug.
        let mut router = instance("openrouter", Some("k"));
        router.account_id = None;
        router.model = Some("perplexity/pplx-decider-flash-v1.3:free".into());
        assert!(build(&router, "k".into()).is_ok());
    }

    /// A profile whose `decision` section holds `entries`, keyed by
    /// name, and no default named.
    fn profile_with(entries: &[(&str, DecisionInstance)]) -> Profile {
        Profile {
            decision: Some(DecisionSettings {
                instance: None,
                instances: entries
                    .iter()
                    .map(|(name, entry)| ((*name).to_owned(), entry.clone()))
                    .collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_default_instance_is_the_provider() {
        let mut clef = instance("clef", Some("k1"));
        clef.model = Some("clef-flash".into());
        let entries = [
            ("clef-work", clef),
            ("jev-main", instance("jev", Some("k2"))),
        ];
        // No default named: the first instance, by name.
        let mut profile = profile_with(&entries);
        assert_eq!(
            resolve_with(|_| None, Some(&profile), None).unwrap().kind(),
            "clef"
        );
        // The default named: that one.
        profile.decision.as_mut().unwrap().instance = Some("jev-main".into());
        let jev = resolve_with(|_| None, Some(&profile), None).unwrap();
        assert_eq!(jev.kind(), "jev");
        // Dangling: does not count, the first again.
        profile.decision.as_mut().unwrap().instance = Some("ghost".into());
        assert_eq!(
            resolve_with(|_| None, Some(&profile), None).unwrap().kind(),
            "clef"
        );
    }

    #[test]
    fn keys_resolve_through_the_keychain_tier() {
        let store = VolatileSecretStore::new();
        store.set("dm-1", "stored-key").unwrap();
        let mut jev = instance("jev", None);
        jev.secret_ref = Some("dm-1".into());
        let profile = profile_with(&[("jev", jev)]);
        let provider = resolve_with(|_| None, Some(&profile), Some(&store)).unwrap();
        assert_eq!(provider.kind(), "jev");
    }

    #[test]
    fn a_default_instance_never_falls_to_another_vendor() {
        // Keyless, and the environment serves the other vendor: still
        // this instance's failure, not Clef's answer.
        let profile = profile_with(&[("jev", instance("jev", None))]);
        let env = |name: &str| match name {
            "CLOUDFLARE_AUTH_TOKEN" => Some("cf".to_owned()),
            "CLOUDFLARE_ACCOUNT_ID" => Some("acc".to_owned()),
            _ => None,
        };
        let err = resolve_with(env, Some(&profile), None)
            .err()
            .expect("no provider expected");
        assert_eq!(
            err,
            ResolveError::Build {
                name: "jev".into(),
                source: BuildError::MissingKey("jev".into())
            }
        );
        assert!(
            err.to_string()
                .contains("decision instance \"jev\": jev needs an api key"),
            "{err}"
        );
        // A kind nothing answers to is unknown, not keyless.
        let profile = profile_with(&[("kagi", instance("kagi", None))]);
        let err = resolve_with(|_| None, Some(&profile), None)
            .err()
            .expect("no provider expected");
        assert_eq!(
            err,
            ResolveError::Build {
                name: "kagi".into(),
                source: BuildError::Unknown("kagi".into())
            }
        );
        // A model the kind lacks is caught at build.
        let mut wrong_model = instance("clef", Some("k"));
        wrong_model.account_id = Some("acc".into());
        wrong_model.model = Some("nope".into());
        let profile = profile_with(&[("cf", wrong_model)]);
        let err = resolve_with(|_| None, Some(&profile), None)
            .err()
            .expect("no provider expected");
        assert!(matches!(
            err,
            ResolveError::Build {
                source: BuildError::UnknownModel { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_default_model_is_what_build_asks_for_a_blank() {
        // Each kind against one wiremock: the request on the wire
        // carries the default the page would show, Clef's on its
        // route as well as in its body.
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "model": "echo", "answers": {"q": {"type": "noul", "noul": 0.5}},
            "usage": {"input_tokens": 1, "output_tokens": 0}
        });
        let clef_body = serde_json::json!({
            "result": body, "success": true, "errors": [], "messages": []
        });
        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/clef$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(clef_body))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        for kind in KINDS {
            let mut blank = instance(kind, Some("k"));
            blank.model = Some("  ".into());
            blank.base_url = Some(server.uri());
            let provider = build(&blank, "k".into()).unwrap();
            let request = rhema_decision::DecisionRequest::new("s")
                .with_question("q", rhema_decision::Question::noul("x?"));
            provider.decide(&request).await.unwrap();
            let received = server.received_requests().await.unwrap();
            let last = received.last().expect("the call landed");
            let sent: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
            let default = default_model(kind).unwrap();
            assert_eq!(
                sent["model"].as_str(),
                Some(default),
                "{kind}: the blank model went out as the default"
            );
            if kind == "clef" {
                assert!(
                    last.url.path().ends_with(default),
                    "the Clef route carries its model: {}",
                    last.url.path()
                );
            }
        }
        assert_eq!(default_model("nope"), None, "an unknown kind has none");
    }

    #[test]
    fn the_environment_tier_walks_the_fixed_kind_order_and_needs_clefs_account() {
        let env = |name: &str| match name {
            "CLOUDFLARE_AUTH_TOKEN" => Some("cf".to_owned()),
            "TYPESAFE_API_KEY" => Some(" ts ".to_owned()),
            _ => None,
        };
        let provider = resolve_with(env, None, None).unwrap();
        assert_eq!(
            provider.kind(),
            "jev",
            "clef has a token but no account; jev's key wins in the fixed order"
        );
        let env = |name: &str| match name {
            "CLOUDFLARE_AUTH_TOKEN" => Some("cf".to_owned()),
            "TYPESAFE_API_KEY" => Some("ts".to_owned()),
            "CLOUDFLARE_ACCOUNT_ID" => Some("acc".to_owned()),
            _ => None,
        };
        assert_eq!(resolve_with(env, None, None).unwrap().kind(), "jev");
        let env = |name: &str| match name {
            "CLOUDFLARE_AUTH_TOKEN" => Some("cf".to_owned()),
            "CLOUDFLARE_ACCOUNT_ID" => Some("acc".to_owned()),
            _ => None,
        };
        assert_eq!(resolve_with(env, None, None).unwrap().kind(), "clef");
        // A token alone is not a Clef provider.
        let env = |name: &str| (name == "CLOUDFLARE_AUTH_TOKEN").then(|| "cf".to_owned());
        assert_eq!(
            resolve_with(env, None, None)
                .err()
                .expect("no provider expected"),
            ResolveError::Unconfigured
        );
        // An empty profile is the environment tier too.
        let profile = Profile::default();
        let env = |name: &str| (name == "TYPESAFE_API_KEY").then(|| "k".to_owned());
        assert_eq!(
            resolve_with(env, Some(&profile), None).unwrap().kind(),
            "jev"
        );
        // The aggregator's key alone serves; with a vendor's key beside
        // it, the vendor comes first in the fixed order.
        let env = |name: &str| (name == "OPENROUTER_API_KEY").then(|| "or".to_owned());
        assert_eq!(resolve_with(env, None, None).unwrap().kind(), "openrouter");
        let env = |name: &str| match name {
            "OPENROUTER_API_KEY" => Some("or".to_owned()),
            "TYPESAFE_API_KEY" => Some("ts".to_owned()),
            _ => None,
        };
        assert_eq!(resolve_with(env, None, None).unwrap().kind(), "jev");
    }

    #[test]
    fn nothing_configured_is_the_unconfigured_error() {
        let err = resolve_with(|_| None, None, None)
            .err()
            .expect("no provider expected");
        assert_eq!(err, ResolveError::Unconfigured);
        assert!(
            err.to_string()
                .contains("no decision instance is configured")
        );
        let err = resolve_with(|_| None, Some(&Profile::default()), None)
            .err()
            .expect("no provider expected");
        assert_eq!(err, ResolveError::Unconfigured);
    }

    #[test]
    fn is_configured_wires_a_held_instance_and_the_environment_tier() {
        assert!(!is_configured_with(|_| None, None, None));
        assert!(!is_configured_with(
            |_| None,
            Some(&Profile::default()),
            None
        ));
        // The environment serves.
        let env = |name: &str| (name == "TYPESAFE_API_KEY").then(|| "k".to_owned());
        assert!(is_configured_with(env, None, None));
        // An instance is held, keyless as it is: the call will say so.
        let profile = profile_with(&[("jev", instance("jev", None))]);
        assert!(is_configured_with(|_| None, Some(&profile), None));
    }

    #[cfg(feature = "mock")]
    #[test]
    fn the_mock_builds_behind_the_same_trait_object() {
        assert_eq!(build_mock("jev").kind(), "jev");
    }
}
