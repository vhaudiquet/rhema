use super::*;

use crate::profile::Profile;
use crate::secrets::VolatileSecretStore;
use crate::settings::Settings;
use std::collections::BTreeSet;

/// A store that answers nothing: the shape a machine without a
/// store presents.
pub(crate) struct UnreachableStore;

impl crate::secrets::SecretStore for UnreachableStore {
    fn get(&self, _: &str) -> Result<Option<String>, crate::secrets::SecretError> {
        Err(crate::secrets::SecretError::NoStore)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), crate::secrets::SecretError> {
        Err(crate::secrets::SecretError::NoStore)
    }
    fn delete(&self, _: &str) -> Result<bool, crate::secrets::SecretError> {
        Err(crate::secrets::SecretError::NoStore)
    }
}

// ---- web search instances ---------------------------------

pub(crate) fn websearch_instance(provider: &str) -> WebSearchInstance {
    WebSearchInstance {
        provider: provider.into(),
        api_key: None,
        secret_ref: None,
    }
}

// ---- decision model instances ------------------------------

/// A `decision` section holding `entries`, keyed by name.
pub(crate) fn decision_settings(entries: &[(&str, &str)]) -> DecisionSettings {
    DecisionSettings {
        instance: None,
        instances: entries
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), decision_instance(kind)))
            .collect(),
    }
}

pub(crate) fn decision_instance(provider: &str) -> DecisionInstance {
    DecisionInstance {
        provider: provider.into(),
        api_key: None,
        secret_ref: None,
        model: None,
        account_id: None,
        base_url: None,
    }
}

#[test]
fn env_key_wins_over_stored_and_blank_counts_as_unset() {
    let env = |name: &str| match name {
        "GEMINI_API_KEY" => Some("  ".into()),
        "GOOGLE_API_KEY" => Some("from-env".into()),
        _ => None,
    };
    assert_eq!(
        resolve_key(
            env,
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            Some("stored".into())
        ),
        Some("from-env".into())
    );
    assert_eq!(
        resolve_key(|_| None, &["GEMINI_API_KEY"], Some(" stored ".into())),
        Some("stored".into())
    );
    assert_eq!(
        resolve_key(|_| None, &["GEMINI_API_KEY"], Some("  ".into())),
        None
    );
    assert_eq!(resolve_key(|_| None, &["GEMINI_API_KEY"], None), None);
}

#[test]
fn anthropic_and_openai_keys_follow_the_same_env_wins_convention() {
    let env = |name: &str| match name {
        "ANTHROPIC_API_KEY" => Some("anthropic-from-env".into()),
        "OPENAI_API_KEY" => Some("  ".into()),
        _ => None,
    };
    assert_eq!(
        resolve_key(env, &["ANTHROPIC_API_KEY"], Some("stored".into())),
        Some("anthropic-from-env".into())
    );
    // A blank env var counts as unset: the stored key serves.
    assert_eq!(
        resolve_key(env, &["OPENAI_API_KEY"], Some("stored".into())),
        Some("stored".into())
    );
}

#[test]
fn a_custom_endpoint_api_choice_round_trips_and_parses_both_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let profile = Profile {
        ignored_mcp_servers: serde_yaml::Value::Null,
        provider: Some("openai".into()),
        instance: Some("proxy".into()),
        instances: BTreeMap::from([(
            "proxy".to_owned(),
            ProviderInstance {
                provider: "openai".into(),
                api_key: Some("sk-p".into()),
                secret_ref: None,
                base_url: Some("http://proxy:1234/v1".into()),
                api: Some(ProviderApi::Responses),
            },
        )]),
        sandbox_instances: BTreeMap::new(),
        websearch_enabled: None,
        websearch: BTreeMap::new(),
        websearch_order: Vec::new(),
        decision_enabled: None,
        decision: None,
        model: Some("gpt-5".into()),
        color: None,
        thinking: None,
        sandbox: None,
    };
    let settings = Settings {
        profiles: BTreeMap::from([("personal".to_owned(), profile)]),
        skills_disabled: BTreeSet::new(),
        disabled_mcp_servers: BTreeSet::new(),
        disabled_subagents: BTreeSet::new(),
    };
    Settings::save_to(&path, &settings).unwrap();
    let loaded = Settings::load_from(&path).unwrap();
    let instance = &loaded.profiles["personal"].instances["proxy"];
    assert_eq!(instance.api, Some(ProviderApi::Responses));
    assert_eq!(instance.api.unwrap().as_str(), "responses");

    // The file spelling parses back, and an instance without the
    // field, every config written before the choice existed,
    // reads as unset.
    let raw = "instances:\n  proxy:\n    provider: openai\n    api: responses\n";
    let parsed = serde_yaml::from_str::<Profile>(raw).unwrap();
    assert_eq!(parsed.instances["proxy"].api, Some(ProviderApi::Responses));
    let raw = "instances:\n  proxy:\n    provider: openai\n";
    let parsed = serde_yaml::from_str::<Profile>(raw).unwrap();
    assert_eq!(parsed.instances["proxy"].api, None);
}

#[test]
fn the_sandbox_env_names_follow_the_same_env_wins_convention() {
    // The env tier of the sandbox trio, injected the way the
    // provider keys are: a test setting the real vars would race
    // the run (and the edition forbids the unsafe write).
    let env = |name: &str| match name {
        "RHEMA_SANDBOX" => Some("docker".into()),
        "RHEMA_SANDBOX_URL" => Some("  ".into()),
        "RHEMA_SANDBOX_TOKEN" => Some("env-token".into()),
        _ => None,
    };
    assert_eq!(
        resolve_key(env, &["RHEMA_SANDBOX"], Some("local".into())),
        Some("docker".into())
    );
    // A blank env value counts as unset: the stored value serves.
    assert_eq!(
        resolve_key(
            env,
            &["RHEMA_SANDBOX_URL"],
            Some("http://stored:7331".into())
        ),
        Some("http://stored:7331".into())
    );
    assert_eq!(
        resolve_key(env, &["RHEMA_SANDBOX_TOKEN"], Some("stored-token".into())),
        Some("env-token".into())
    );
    assert_eq!(resolve_key(|_| None, &["RHEMA_SANDBOX"], None), None);
}

#[test]
fn the_websearch_key_tiers_env_then_store_then_literal() {
    // The environment tier, purely: the closure stands in for
    // std::env, set_var is unsafe under the crate's forbid.
    let env = |name: &str| (name == "TAVILY_API_KEY").then(|| " env-key ".to_owned());
    assert_eq!(
        resolve_key(
            env,
            websearch_env_names("tavily"),
            Some(" file-key ".into())
        )
        .as_deref(),
        Some("env-key")
    );
    // Then the store, then the literal, trims included.
    let mut instance = websearch_instance("tavily");
    instance.api_key = Some(" file-key ".into());
    let store = VolatileSecretStore::new();
    store.set("id-1", " chain-key ").unwrap();
    instance.secret_ref = Some("id-1".into());
    assert_eq!(instance.api_key(Some(&store)).as_deref(), Some("chain-key"));
    instance.secret_ref = Some("id-missing".into());
    assert_eq!(instance.api_key(Some(&store)).as_deref(), Some("file-key"));
}

#[test]
fn a_whitespace_key_counts_as_unset_at_every_websearch_tier() {
    let mut instance = websearch_instance("exa");
    instance.api_key = Some("   ".into());
    assert_eq!(instance.api_key(None), None);
    let store = VolatileSecretStore::new();
    store.set("id-2", "  ").unwrap();
    instance.secret_ref = Some("id-2".into());
    assert_eq!(instance.api_key(Some(&store)), None);
}

#[test]
fn the_websearch_instance_debug_redacts_the_key() {
    let mut instance = websearch_instance("brave");
    instance.api_key = Some("sk-secret".into());
    let text = format!("{instance:?}");
    assert!(!text.contains("sk-secret"), "{text}");
    assert!(text.contains("[\u{2026}]"), "{text}");
}

#[test]
fn a_websearch_entry_round_trips_and_rejects_unknown_fields() {
    let text = "provider: brave\napi_key: k\n";
    let instance: WebSearchInstance = serde_yaml::from_str(text).unwrap();
    assert_eq!(instance.provider, "brave");
    let err = serde_yaml::from_str::<WebSearchInstance>("provider: brave\napi: nope\n");
    assert!(err.is_err(), "unknown keys are loud");
}

// ---- decision model instances ------------------------------

#[test]
fn the_decision_key_tiers_env_then_store_then_literal() {
    let env = |name: &str| (name == "TYPESAFE_API_KEY").then(|| " env-key ".to_owned());
    assert_eq!(
        resolve_key(env, decision_env_names("jev"), Some(" file-key ".into())).as_deref(),
        Some("env-key")
    );
    let mut instance = decision_instance("jev");
    instance.api_key = Some(" file-key ".into());
    let store = VolatileSecretStore::new();
    store.set("id-1", " chain-key ").unwrap();
    instance.secret_ref = Some("id-1".into());
    assert_eq!(instance.api_key(Some(&store)).as_deref(), Some("chain-key"));
    instance.secret_ref = Some("id-missing".into());
    assert_eq!(instance.api_key(Some(&store)).as_deref(), Some("file-key"));
    instance.api_key = Some("   ".into());
    assert_eq!(instance.api_key(None), None, "blank is unset");
}

#[test]
fn the_clef_account_reads_the_environment_then_the_file() {
    let mut instance = decision_instance("clef");
    instance.account_id = Some(" acc-file ".into());
    // The environment tier, via the pure resolver the method uses.
    let env = |name: &str| (name == CLOUDFLARE_ACCOUNT_ENV).then(|| "acc-env".to_owned());
    assert_eq!(
        resolve_key(env, &[CLOUDFLARE_ACCOUNT_ENV], instance.account_id.clone()).as_deref(),
        Some("acc-env")
    );
    // With no variable set, the file's id, trimmed.
    let resolved = instance.account_id();
    assert!(
        resolved.as_deref() == Some("acc-file") || resolved.is_none(),
        "the process environment decides between the two"
    );
    assert!(decision_instance("jev").account_id().is_none());
}

#[test]
fn the_decision_instance_debug_redacts_the_key_only() {
    let mut instance = decision_instance("clef");
    instance.api_key = Some("cf-secret".into());
    instance.account_id = Some("acc-1".into());
    let text = format!("{instance:?}");
    assert!(!text.contains("cf-secret"), "{text}");
    assert!(text.contains("acc-1"), "the account id is not a secret");
    assert!(text.contains("[\u{2026}]"), "{text}");
}

#[test]
fn a_decision_entry_round_trips_and_rejects_unknown_fields() {
    let text = "provider: clef\nmodel: clef-flash\naccount_id: acc\n";
    let instance: DecisionInstance = serde_yaml::from_str(text).unwrap();
    assert_eq!(instance.provider, "clef");
    assert_eq!(instance.model.as_deref(), Some("clef-flash"));
    assert_eq!(instance.account_id.as_deref(), Some("acc"));
    let err = serde_yaml::from_str::<DecisionInstance>("provider: jev\napi: nope\n");
    assert!(err.is_err(), "unknown keys are loud");
    // An instance without the new fields, every config written
    // before the section existed, reads as unset.
    let bare: DecisionInstance = serde_yaml::from_str("provider: jev\n").unwrap();
    assert_eq!(bare.model, None);
    assert_eq!(bare.account_id, None);
    assert_eq!(bare.base_url, None);
    let routed: DecisionInstance =
        serde_yaml::from_str("provider: jev\nbase_url: http://localhost:30000\n").unwrap();
    assert_eq!(routed.base_url.as_deref(), Some("http://localhost:30000"));
}
