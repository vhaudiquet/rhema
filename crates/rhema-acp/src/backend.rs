//! Backend resolution: which provider and model serve ACP turns.
//!
//! The same startup resolution the headless one-shot performs — a
//! provider choice (flag, then `RHEMA_PROVIDER` over the stored
//! config), a model (flag, then stored, then the kind's fallback), and
//! whatever credentials the profile holds. `Ok(None)` means no
//! credential resolved: to the client that is `auth_required`, the one
//! honest answer. An `Err` names a configuration problem — nothing
//! configured, or a stored spelling that names no known kind; nothing
//! defaults to a provider the user never picked.

use std::sync::Arc;

use rhema_config::Profile;
use rhema_config::secrets::SecretStore;
use rhema_core::{ModelProvider, ProviderError};
use rhema_provider::ModelCard;
use rhema_provider_registry::build;

/// The provider kinds rhema serves, by their config names. The model
/// select's choice values are `<kind>/<model>` on these names, so a
/// switch can move a session across providers, and the catalog spans
/// every kind whose credentials resolve.
pub const PROVIDERS: &[&str] = &[
    "llama-cpp",
    "openrouter",
    "anthropic",
    "openai",
    "deepseek",
    "zai",
    "copilot",
    "google",
];

/// The [`PROVIDERS`] name a stored or flagged kind reads as; anything
/// unknown is an error, not a default — see [`resolve`].
fn known_kind(kind: &str) -> Result<&'static str, String> {
    PROVIDERS
        .iter()
        .find(|known| **known == kind)
        .copied()
        .ok_or_else(|| format!("unknown provider kind \"{kind}\" in the stored config"))
}

/// A backend that can serve turns: which provider kind it rides on,
/// the provider implementation, and the model to run.
pub struct Resolved {
    /// The provider kind behind the agent — a [`PROVIDERS`] name, or
    /// `"mock"` under the mock feature.
    pub kind: &'static str,
    /// The provider implementation behind the agent.
    pub provider: Arc<dyn ModelProvider>,
    /// The model every turn runs on.
    pub model: String,
}

/// One model a client's picker may pick: the provider kind it rides on
/// (a [`PROVIDERS`] name) and the catalog row. A choice's select value
/// is `<provider>/<card id>`.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    /// The provider kind this model runs on.
    pub provider: &'static str,
    /// The model card as the listing hands it out.
    pub card: ModelCard,
}

/// Every logged-in provider's catalog: for each kind in [`PROVIDERS`]
/// whose credentials resolve — `llama-cpp` always — its text-chat rows
/// fetched over its API, all kinds concurrently, sorted by provider
/// then id. The same set the TUI's `/model` picker offers. A kind
/// without resolvable credentials is silently absent (it was never
/// logged in); a fetch that fails warns on stderr and the rest still
/// land.
pub async fn catalog(
    profile: &Profile,
    llama_url: Option<&str>,
    secrets: Option<&dyn SecretStore>,
) -> Vec<CatalogRow> {
    let mut tasks = Vec::new();
    for kind in PROVIDERS {
        if let Some(task) = catalog_fetch(kind, profile, llama_url, secrets) {
            tasks.push((*kind, task));
        }
    }
    let mut rows = Vec::new();
    for (kind, task) in tasks {
        match task.await {
            Ok(Ok(cards)) => {
                rows.extend(cards.into_iter().map(|card| CatalogRow {
                    provider: kind,
                    card,
                }));
            }
            Ok(Err(err)) => {
                eprintln!("rhema-acp: warning: {kind}'s catalog was not fetched: {err}");
            }
            Err(err) => {
                eprintln!("rhema-acp: warning: {kind}'s catalog fetch did not finish: {err}");
            }
        }
    }
    rows.sort_by(|a, b| a.provider.cmp(b.provider).then(a.card.id.cmp(&b.card.id)));
    rows
}

/// Spawns one kind's catalog fetch; `None` when the kind is not logged
/// in (its credentials do not resolve) — nothing to fetch, nothing to
/// say. The list_models calls are the adapters' own, so each arm
/// builds its concrete provider here rather than through the seam.
fn catalog_fetch(
    kind: &'static str,
    profile: &Profile,
    llama_url: Option<&str>,
    secrets: Option<&dyn SecretStore>,
) -> Option<tokio::task::JoinHandle<Result<Vec<ModelCard>, ProviderError>>> {
    let profile = profile.clone();
    let llama_url = llama_url
        .map(str::to_owned)
        .unwrap_or_else(|| profile.llama_cpp_url());
    // A stored route override is the zai kind's alone; handing it to
    // the registry unguarded would read it as an openai or deepseek
    // override too.
    let route = (kind == "zai")
        .then(|| stored_zai_route(&profile))
        .flatten();
    let key = profile.api_key(kind, secrets);
    let built = match kind {
        // The server address is optional to guard with a key; every
        // other kind is keyed, and no credential means nothing to
        // fetch — `None`, the same answer an unresolvable turn gets.
        "llama-cpp" => build(kind, key.as_deref(), Some(&llama_url), None),
        _ => {
            let key = key.as_deref()?;
            build(kind, Some(key), None, route.as_deref())
        }
    }
    .expect("a PROVIDERS kind the registry has no wiring for");
    Some(tokio::spawn(
        async move { built.listing.list_models().await },
    ))
}

/// The stored route override of the profile's zai instance, trimmed and
/// blank-filtered — the shape [`Profile::instance_url`] serves.
fn stored_zai_route(profile: &Profile) -> Option<String> {
    profile
        .instance_for_provider("zai")
        .and_then(|(_, instance)| profile.instance_url(instance))
}

/// Resolves the backend the profile's settings call for. `provider`
/// overrides the stored choice; `model` the stored model; `llama_url`
/// the stored server URL. The stored choice reads through
/// [`Profile::provider_name`], so `RHEMA_PROVIDER` wins over the stored
/// `provider`, the same env-wins convention the keys follow. `Ok(None)`
/// when the chosen kind has no usable credentials — `auth_required` to
/// the client. An `Err` names the configuration problem: nothing
/// configured at all, or a spelling that names no known kind.
pub fn resolve(
    profile: &Profile,
    provider: Option<&str>,
    model: Option<&str>,
    llama_url: Option<&str>,
    secrets: Option<&dyn SecretStore>,
) -> Result<Option<Resolved>, String> {
    let stored = profile.provider_name();
    let chosen = provider
        .map(str::to_owned)
        .or(stored)
        .ok_or_else(|| NO_PROVIDER_CONFIGURED.to_owned())?;
    let kind = known_kind(&chosen)?;
    let model = model
        .map(str::to_owned)
        .or_else(|| profile.model.clone())
        // The registry owns the fallback: one table, every frontend.
        .unwrap_or_else(|| rhema_config::default_model(kind).to_owned());
    let provider = provider_for(kind, profile, llama_url, secrets)?;
    Ok(provider.map(|provider| Resolved {
        kind,
        provider,
        model,
    }))
}

/// The fix list a resolution with nothing configured reports. The
/// spelled-out fixes are the point: nothing defaults, so the error is
/// the only guidance a headless or editor-side session gets.
const NO_PROVIDER_CONFIGURED: &str = "no provider configured: set RHEMA_PROVIDER, log in with the rhema TUI, \
     or store one in a profile";

/// The provider a kind's turns ride on: the raw adapter under the
/// shared retry, so a transient failure costs a pause instead of the
/// session (see `rhema_provider::Retrying`). `Ok(None)` when the kind's
/// credentials do not resolve; `llama-cpp` is keyless and always
/// resolves. A kind outside [`PROVIDERS`] is an error naming the
/// stored value, never a silent google.
pub fn provider_for(
    kind: &str,
    profile: &Profile,
    llama_url: Option<&str>,
    secrets: Option<&dyn SecretStore>,
) -> Result<Option<Arc<dyn ModelProvider>>, String> {
    // An unknown spelling is an error naming it, never a silent
    // `auth_required` — the same gate `resolve` applies. Credential
    // resolution stays here, settings vocabulary; the registry's rows
    // are wiring only. A keyed kind with no key is `Ok(None)` (the
    // client reads `auth_required`).
    let kind = known_kind(kind)?;
    let llama_url = llama_url
        .map(str::to_owned)
        .unwrap_or_else(|| profile.llama_cpp_url());
    let route = (kind == "zai").then(|| stored_zai_route(profile)).flatten();
    let key = profile.api_key(kind, secrets);
    let built = match kind {
        "llama-cpp" => build(kind, key.as_deref(), Some(&llama_url), None),
        _ => {
            let Some(key) = key.as_deref() else {
                // No key, no provider — `auth_required` to the client.
                return Ok(None);
            };
            build(kind, Some(key), None, route.as_deref())
        }
    }
    .map_err(|err| err.to_string())?;
    Ok(Some(Arc::new(rhema_provider::Retrying::new(
        built.provider,
    ))))
}

/// Splits a model select value into the provider kind to ride on and
/// the model to run. `<kind>/<model>` names the kind, and the rest of
/// the value is the model id — which may itself carry slashes
/// (OpenRouter's `vendor/model` ids). A bare value stays on the
/// session's current kind, and so does a `<current kind>/…` prefix
/// (`mock`, under the mock feature). A slash value whose first segment
/// names neither a [`PROVIDERS`] kind nor the current kind is a client
/// bug and errors, rather than leaking as a model id.
pub fn split_switch(
    value: &str,
    current_kind: &str,
) -> Result<(Option<&'static str>, String), String> {
    let Some((head, model)) = value.split_once('/') else {
        return Ok((None, value.to_owned()));
    };
    if let Some(kind) = PROVIDERS.iter().find(|known| **known == head) {
        return Ok((Some(kind), model.to_owned()));
    }
    if head == current_kind {
        return Ok((None, model.to_owned()));
    }
    Err(format!(
        "unknown provider kind \"{head}\" in model choice \"{value}\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_config::ProviderInstance;

    fn profile(provider: Option<&str>, model: Option<&str>) -> Profile {
        Profile {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            ..Profile::default()
        }
    }

    #[test]
    fn the_keyless_backend_always_resolves() {
        let resolved = resolve(&profile(Some("llama-cpp"), None), None, None, None, None)
            .expect("llama-cpp is keyless and always resolves")
            .expect("llama-cpp is keyless and always resolves");
        assert_eq!(resolved.model, "local");
    }

    #[test]
    fn the_flags_override_the_stored_choices() {
        let resolved = resolve(
            &profile(Some("openrouter"), Some("custom-model")),
            Some("llama-cpp"),
            Some("flag-model"),
            Some("http://localhost:9999"),
            None,
        )
        .expect("llama-cpp is keyless and always resolves")
        .expect("llama-cpp is keyless and always resolves");
        assert_eq!(resolved.model, "flag-model");
    }

    #[test]
    fn the_model_falls_back_config_then_kind_default() {
        let stored = resolve(
            &profile(Some("llama-cpp"), Some("stored-model")),
            None,
            None,
            None,
            None,
        )
        .expect("llama-cpp is keyless and always resolves")
        .expect("llama-cpp is keyless and always resolves");
        assert_eq!(stored.model, "stored-model");
    }

    #[test]
    fn an_explicit_choice_to_a_keyed_kind_can_only_fail() {
        // Without a resolvable key an explicit openrouter choice is
        // `Ok(None)` — the serve layer answers `auth_required`. A key in
        // the sandbox env would flip this; CI keeps the env bare.
        if std::env::var_os("OPENROUTER_API_KEY").is_some() {
            return;
        }
        assert!(matches!(
            resolve(&profile(None, None), Some("openrouter"), None, None, None),
            Ok(None)
        ));
    }

    #[test]
    fn nothing_configured_is_an_error_naming_the_fixes() {
        // No flag, no stored provider: an error listing the fixes, not a
        // silent google. (An exported RHEMA_PROVIDER in the suite's env
        // would count as configured; CI keeps the env bare.)
        if std::env::var_os("RHEMA_PROVIDER").is_some() {
            return;
        }
        let err = match resolve(&profile(None, None), None, None, None, None) {
            Ok(_) => panic!("nothing is configured, yet a backend resolved"),
            Err(err) => err,
        };
        assert!(
            err.contains("RHEMA_PROVIDER"),
            "the error names the fixes: {err}"
        );
    }

    #[test]
    fn an_unknown_stored_spelling_is_an_error_naming_the_value() {
        if std::env::var_os("RHEMA_PROVIDER").is_some() {
            return;
        }
        let err = match resolve(&profile(Some("nonsense"), None), None, None, None, None) {
            Ok(_) => panic!("an unknown spelling resolved a backend"),
            Err(err) => err,
        };
        assert!(err.contains("nonsense"), "the error names the value: {err}");
        // An explicit unknown kind errors the same way.
        let err = match resolve(&profile(None, None), Some("bogus"), None, None, None) {
            Ok(_) => panic!("an unknown flagged kind resolved a backend"),
            Err(err) => err,
        };
        assert!(err.contains("bogus"), "the error names the value: {err}");
    }

    #[test]
    fn the_copilot_kind_resolves_from_a_stored_token() {
        let mut profile = Profile {
            provider: Some("copilot".into()),
            ..Profile::default()
        };
        profile.upsert_instance(
            "copilot",
            ProviderInstance {
                provider: "copilot".into(),
                api_key: Some("gho_token".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        let resolved = resolve(&profile, None, None, None, None)
            .expect("a stored token resolves")
            .expect("a stored token resolves");
        assert_eq!(resolved.kind, "copilot");
        assert_eq!(resolved.model, "gpt-4.1");
    }

    #[test]
    fn the_zai_kind_resolves_from_a_stored_key_and_its_route() {
        let mut profile = Profile {
            provider: Some("zai".into()),
            ..Profile::default()
        };
        profile.upsert_instance(
            "zai",
            ProviderInstance {
                provider: "zai".into(),
                api_key: Some("zkey".into()),
                secret_ref: None,
                base_url: Some("https://proxy.example/api/anthropic".into()),
            },
        );
        let resolved = resolve(&profile, None, None, None, None)
            .expect("a stored key resolves")
            .expect("a stored key resolves");
        assert_eq!(resolved.kind, "zai");
        // The kind's own fallback model, not the anthropic one.
        assert_eq!(resolved.model, "GLM-5.3");
        // No key, no provider — `auth_required` to the client.
        let mut keyless = Profile {
            provider: Some("zai".into()),
            ..Profile::default()
        };
        keyless.upsert_instance(
            "zai",
            ProviderInstance {
                provider: "zai".into(),
                api_key: None,
                secret_ref: None,
                base_url: None,
            },
        );
        assert!(
            provider_for("zai", &keyless, None, None)
                .expect("a known kind resolves")
                .is_none(),
            "a keyless zai kind is auth_required, not an error"
        );
    }

    #[test]
    fn a_provider_for_an_unknown_kind_is_an_error_not_google() {
        let mut keyed = Profile::default();
        keyed.upsert_instance(
            "google",
            ProviderInstance {
                provider: "google".into(),
                api_key: Some("g".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        let err = match provider_for("nonsense", &keyed, None, None) {
            Ok(_) => panic!("an unknown kind built a provider"),
            Err(err) => err,
        };
        assert!(err.contains("nonsense"), "the error names the kind: {err}");
    }

    #[test]
    fn a_kind_prefixed_choice_names_the_kind_and_keeps_the_slashed_id() {
        let (kind, model) =
            split_switch("openrouter/z-ai/glm-5.3", "llama-cpp").expect("a kind switch");
        assert_eq!(kind, Some("openrouter"));
        assert_eq!(model, "z-ai/glm-5.3");
    }

    #[test]
    fn a_bare_choice_stays_on_the_current_kind() {
        let (kind, model) = split_switch("mirai-s-27b", "llama-cpp").expect("a bare choice");
        assert_eq!(kind, None);
        assert_eq!(model, "mirai-s-27b");
    }

    #[test]
    fn the_current_kind_prefix_is_taken_off_without_a_rebuild() {
        // `mock` is not a PROVIDERS name: the mock feature's sessions
        // switch models through their own kind prefix.
        let (kind, model) = split_switch("mock/other-model", "mock").expect("a mock choice");
        assert_eq!(kind, None);
        assert_eq!(model, "other-model");
    }

    #[test]
    fn a_kind_prefix_that_is_the_current_kind_still_resolves() {
        let (kind, model) =
            split_switch("llama-cpp/local", "llama-cpp").expect("a same-kind switch");
        assert_eq!(kind, Some("llama-cpp"));
        assert_eq!(model, "local");
    }

    #[test]
    fn an_unknown_kind_prefix_is_an_error_not_a_model_id() {
        let err = split_switch("bogus/model", "llama-cpp").expect_err("an unknown kind");
        assert!(err.contains("bogus"));
    }

    #[test]
    fn a_prefix_that_merely_starts_with_a_kind_name_is_not_that_kind() {
        let err = split_switch("openrouterish/model", "llama-cpp").expect_err("a near miss");
        assert!(err.contains("openrouterish"));
    }
}
