use super::*;

use crate::instances::ProviderInstance;
use crate::instances::tests::{UnreachableStore, decision_settings, websearch_instance};
use crate::secrets::{KeyringSecretReader, VolatileSecretStore};
use rhema_core::request::ThinkingEffort;

/// A test-only legacy source: the volatile store's map behind the
/// read-only seam, plus a call count so a test can prove a ref the
/// file store already holds never reaches the keyring at all.
struct LegacyMap {
    values: VolatileSecretStore,
    calls: std::sync::atomic::AtomicUsize,
}

impl LegacyMap {
    fn new() -> Self {
        Self {
            values: VolatileSecretStore::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl crate::secrets::LegacySecretStore for LegacyMap {
    fn get(&self, id: &str) -> Result<Option<String>, crate::secrets::SecretError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::secrets::SecretStore::get(&self.values, id)
    }
}

#[test]
fn disabled_skills_round_trip_and_default_to_quiet() {
    // No skill is off by default.
    assert!(Settings::default().skills_disabled.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let settings = Settings {
        skills_disabled: BTreeSet::from(["commit".to_owned(), "review".to_owned()]),
        ..Settings::default()
    };
    Settings::save_to(&path, &settings).unwrap();
    let loaded = Settings::load_from(&path).unwrap();
    assert_eq!(
        loaded.skills_disabled,
        BTreeSet::from(["commit".to_owned(), "review".to_owned()])
    );
    // A file written before the field existed keeps reading as the
    // quiet state.
    std::fs::write(&path, "profiles: {}\n").unwrap();
    let old = Settings::load_from(&path).unwrap();
    assert!(old.skills_disabled.is_empty());
}

#[test]
fn disabled_subagents_round_trip_and_default_to_quiet() {
    // No subagent is off by default.
    assert!(Settings::default().disabled_subagents.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let settings = Settings {
        disabled_subagents: BTreeSet::from(["scout".to_owned(), "reviewer".to_owned()]),
        ..Settings::default()
    };
    Settings::save_to(&path, &settings).unwrap();
    let loaded = Settings::load_from(&path).unwrap();
    assert_eq!(
        loaded.disabled_subagents,
        BTreeSet::from(["scout".to_owned(), "reviewer".to_owned()])
    );
    // A file written before the field existed keeps reading as the
    // quiet state.
    std::fs::write(&path, "profiles: {}\n").unwrap();
    let old = Settings::load_from(&path).unwrap();
    assert!(old.disabled_subagents.is_empty());
}

#[test]
fn config_round_trips_through_disk_as_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let config = Settings {
        profiles: BTreeMap::from([(
            "personal".to_owned(),
            Profile {
                ignored_mcp_servers: serde_yaml::Value::Null,
                sandbox_instances: BTreeMap::new(),
                websearch_enabled: None,
                websearch: BTreeMap::new(),
                websearch_order: Vec::new(),
                decision_enabled: None,
                decision: None,
                provider: Some("llama-cpp".into()),
                instance: Some("home server".into()),
                instances: BTreeMap::from([
                    (
                        "home server".to_owned(),
                        ProviderInstance {
                            provider: "llama-cpp".into(),
                            api_key: None,
                            secret_ref: None,
                            base_url: Some("http://localhost:8080".into()),
                            api: None,
                        },
                    ),
                    (
                        "anthropic".to_owned(),
                        ProviderInstance {
                            provider: "anthropic".into(),
                            api_key: Some("sk-ant-123".into()),
                            secret_ref: None,
                            base_url: None,
                            api: None,
                        },
                    ),
                    (
                        "copilot".to_owned(),
                        ProviderInstance {
                            provider: "copilot".into(),
                            api_key: Some("gho-123".into()),
                            secret_ref: None,
                            base_url: None,
                            api: None,
                        },
                    ),
                    (
                        "deepseek".to_owned(),
                        ProviderInstance {
                            provider: "deepseek".into(),
                            api_key: Some("sk-dsk-123".into()),
                            secret_ref: None,
                            base_url: None,
                            api: None,
                        },
                    ),
                ]),
                model: Some("gemini-3.8-flash".into()),
                color: None,
                thinking: Some("low".into()),
                sandbox: None,
            },
        )]),
        skills_disabled: BTreeSet::new(),
        disabled_mcp_servers: BTreeSet::new(),
        disabled_subagents: BTreeSet::new(),
    };
    Settings::save_to(&path, &config).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("instances:"), "yaml on disk: {raw}");
    assert!(raw.contains("sk-ant-123"), "yaml on disk: {raw}");
    assert!(raw.contains("gho-123"), "yaml on disk: {raw}");
    assert!(raw.contains("sk-dsk-123"), "yaml on disk: {raw}");
    assert!(!raw.contains('{'), "not json: {raw}");
    let loaded = Settings::load_from(&path).unwrap();
    assert_eq!(loaded, config);

    // The thinking level parses; unset and junk read as the medium
    // default, an explicit `default` keeps the provider's own.
    let mut config = config;
    assert_eq!(
        config.profiles["personal"].thinking_effort(),
        ThinkingEffort::Low
    );
    config.profiles.get_mut("personal").unwrap().thinking = Some("maximum".into());
    assert_eq!(
        config.profiles["personal"].thinking_effort(),
        ThinkingEffort::Medium
    );
    config.profiles.get_mut("personal").unwrap().thinking = None;
    assert_eq!(
        config.profiles["personal"].thinking_effort(),
        ThinkingEffort::Medium
    );
    config.profiles.get_mut("personal").unwrap().thinking = Some("default".into());
    assert_eq!(
        config.profiles["personal"].thinking_effort(),
        ThinkingEffort::Default
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
    }
}

#[test]
fn a_missing_file_loads_as_the_default_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.yaml");
    assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
}

#[test]
fn fresh_setups_ship_personal_and_work_profiles() {
    let config = Settings::default();
    assert!(config.profiles.contains_key("personal"));
    assert!(config.profiles.contains_key("work"));
    // A loaded file missing the key gets the same seeding.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "skills_disabled: [commit]\n").unwrap();
    let loaded = Settings::load_from(&path).unwrap();
    assert!(loaded.profiles.contains_key("work"));
    assert_eq!(
        loaded.skills_disabled,
        BTreeSet::from(["commit".to_owned()])
    );
}

#[test]
fn an_unknown_key_is_rejected_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "modle: claude-sonnet-4-5\n").unwrap();
    let err = Settings::load_from(&path).expect_err("a typo'd key must not read as a default");
    let message = err.to_string();
    assert!(
        message.contains("modle") && message.contains("expected one of"),
        "the error names the key: {message}"
    );
    // The same strictness inside a profile.
    std::fs::write(&path, "profiles:\n  personal:\n    provder: anthropic\n").unwrap();
    let err = Settings::load_from(&path).expect_err("a typo'd profile key is loud too");
    assert!(err.to_string().contains("provder"), "got: {err}");
}

#[test]
fn profiles_are_isolated_from_each_other() {
    let keyed = |provider: &str, key: &str| {
        let mut profile = Profile {
            provider: Some(provider.to_owned()),
            ..Profile::default()
        };
        profile.upsert_instance(
            provider,
            ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some(key.to_owned()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        profile
    };
    let config = Settings {
        profiles: BTreeMap::from([
            (
                "personal".to_owned(),
                Profile {
                    model: Some("personal-model".into()),
                    ..keyed("google", "personal-key")
                },
            ),
            (
                "work".to_owned(),
                Profile {
                    color: Some("#8FA6B8".into()),
                    ..keyed("openrouter", "work-key")
                },
            ),
        ]),
        skills_disabled: BTreeSet::new(),
        disabled_mcp_servers: BTreeSet::new(),
        disabled_subagents: BTreeSet::new(),
    };
    let work = config.effective(Some("work"));
    assert_eq!(work.provider.as_deref(), Some("openrouter"));
    assert_eq!(work.stored_provider_key("google", None).as_deref(), None);
    assert_eq!(
        work.stored_provider_key("openrouter", None).as_deref(),
        Some("work-key")
    );
    assert_eq!(work.model, None);
    assert_eq!(work.color.as_deref(), Some("#8FA6B8"));

    // personal's fields never leak into work, and vice versa.
    let personal = config.effective(Some("personal"));
    assert_eq!(
        personal.stored_provider_key("google", None).as_deref(),
        Some("personal-key")
    );
    assert_eq!(personal.model.as_deref(), Some("personal-model"));

    // An unknown name resolves to an empty profile: nothing leaks in.
    assert_eq!(config.effective(Some("other")), Profile::default());
}

#[test]
fn set_for_profile_writes_the_named_section() {
    let mut config = Settings::default();
    config.set_for_profile(Some("work"), |p| {
        p.upsert_instance(
            "google",
            ProviderInstance {
                provider: "google".into(),
                api_key: Some("work-key".into()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
    });
    // Unflagged writes land on personal, the default profile.
    config.set_for_profile(None, |p| p.model = Some("m".into()));
    assert_eq!(
        config.profiles["work"]
            .stored_provider_key("google", None)
            .as_deref(),
        Some("work-key")
    );
    assert_eq!(config.profiles["personal"].model.as_deref(), Some("m"));
    // work's section is untouched by personal's writes.
    assert_eq!(config.profiles["work"].model, None);
}

#[test]
fn migration_moves_literals_into_the_store_and_round_trips() {
    use crate::secrets::VolatileSecretStore;
    let keyed = |provider: &str, key: &str| {
        let mut profile = Profile {
            provider: Some(provider.to_owned()),
            ..Profile::default()
        };
        profile.upsert_instance(
            provider,
            ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some(key.to_owned()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        profile
    };
    let mut config = Settings {
        profiles: BTreeMap::from([
            ("personal".to_owned(), keyed("google", "personal-key")),
            ("work".to_owned(), keyed("openrouter", "work-key")),
        ]),
        skills_disabled: BTreeSet::new(),
        disabled_mcp_servers: BTreeSet::new(),
        disabled_subagents: BTreeSet::new(),
    };
    let store = VolatileSecretStore::new();
    assert_eq!(config.migrate_secrets(&store), 2);
    for (name, expected) in [("google", "personal-key"), ("openrouter", "work-key")] {
        let instance = config
            .profiles
            .values()
            .flat_map(|p| p.instances.values())
            .find(|instance| instance.provider == name)
            .unwrap();
        assert_eq!(instance.api_key, None, "{name}'s literal is gone");
        let id = instance.secret_ref.as_deref().unwrap();
        assert_eq!(store.get(id).unwrap().as_deref(), Some(expected));
    }
    // Idempotent: nothing left to move.
    assert_eq!(config.migrate_secrets(&store), 0);
    // The refs survive the file round trip.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    Settings::save_to(&path, &config).unwrap();
    let mut loaded = Settings::load_from(&path).unwrap();
    assert_eq!(loaded, config);
    assert_eq!(loaded.migrate_secrets(&store), 0);
}

#[test]
fn a_store_that_refuses_everything_keeps_the_literals() {
    let mut config = Settings::default();
    config.set_for_profile(Some("personal"), |p| {
        p.upsert_instance(
            "google",
            ProviderInstance {
                provider: "google".into(),
                api_key: Some("stays-put".into()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
    });
    assert_eq!(config.migrate_secrets(&UnreachableStore), 0);
    let instance = config.profiles["personal"]
        .instance_for_provider("google")
        .unwrap()
        .1;
    assert_eq!(instance.api_key.as_deref(), Some("stays-put"));
    assert_eq!(instance.secret_ref, None);
}

#[test]
fn foreign_keys_survive_a_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        "icons: nerd\ntheme: dark\nprofiles:\n  personal:\n    provider: google\n",
    )
    .expect("writes the file");
    let mut document = SettingsDocument::load_from(&path).expect("loads");
    assert_eq!(
        document.rest.get("icons").and_then(|value| value.as_str()),
        Some("nerd")
    );
    assert!(document.settings.profiles.contains_key("personal"));
    // A mutation through the section, saved: the terminal's keys
    // are still in the file, and the section's change is in it too.
    document.settings.skills_disabled.insert("old-skill".into());
    SettingsDocument::save_to(&path, &document).expect("saves");
    let raw = std::fs::read_to_string(&path).expect("reads");
    assert!(raw.contains("icons: nerd"), "{raw}");
    assert!(raw.contains("theme: dark"), "{raw}");
    assert!(raw.contains("old-skill"), "{raw}");
    let again = SettingsDocument::load_from(&path).expect("reloads");
    assert_eq!(again, document);
}

#[test]
fn a_missing_file_reads_as_the_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let document = SettingsDocument::load_from(&path).expect("loads");
    assert_eq!(document, SettingsDocument::default());
}

#[test]
fn bad_values_in_the_section_still_fail_the_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "icons: nerd\nprofiles: not-a-map\n").expect("writes the file");
    assert!(SettingsDocument::load_from(&path).is_err());
}

#[test]
fn migrate_secrets_moves_websearch_literals_into_the_chain() {
    let mut settings = Settings::default();
    let mut profile = Profile::default();
    let mut instance = websearch_instance("exa");
    instance.api_key = Some("plain".into());
    profile.websearch.insert("exa-work".into(), instance);
    settings.profiles.insert("personal".into(), profile);
    let store = VolatileSecretStore::new();
    assert_eq!(settings.migrate_secrets(&store), 1);
    let instance = &settings.profiles["personal"].websearch["exa-work"];
    assert_eq!(instance.api_key, None);
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("plain"));
    // Idempotent: a second run moves nothing.
    assert_eq!(settings.migrate_secrets(&store), 0);
}

#[test]
fn migrate_secrets_moves_the_decision_literals_into_the_store() {
    let mut settings = Settings::default();
    let mut profile = Profile::default();
    let mut decision = decision_settings(&[("clef-work", "clef"), ("jev", "jev")]);
    let clef = decision.instances.get_mut("clef-work").unwrap();
    clef.api_key = Some("plain".into());
    clef.account_id = Some("acc".into());
    // A second literal moves too, and a keyless one costs nothing.
    decision.instances.get_mut("jev").unwrap().api_key = Some(" ".into());
    profile.decision = Some(decision);
    settings.profiles.insert("personal".into(), profile);
    let store = VolatileSecretStore::new();
    assert_eq!(settings.migrate_secrets(&store), 1);
    let instance = &settings.profiles["personal"]
        .decision
        .as_ref()
        .unwrap()
        .instances["clef-work"];
    assert_eq!(instance.api_key, None);
    assert_eq!(
        instance.account_id.as_deref(),
        Some("acc"),
        "not a secret, kept"
    );
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("plain"));
    assert_eq!(settings.migrate_secrets(&store), 0, "idempotent");
}

#[test]
fn old_keyring_refs_are_copied_without_touching_the_config() {
    let mut settings = Settings::default();
    let mut profile = Profile::default();
    profile.instances.insert(
        "google".into(),
        ProviderInstance {
            provider: "google".into(),
            api_key: None,
            secret_ref: Some("ref-provider".into()),
            base_url: None,
            api: None,
        },
    );
    profile.instances.insert(
        "missing".into(),
        ProviderInstance {
            provider: "openai".into(),
            api_key: None,
            secret_ref: Some("ref-missing".into()),
            base_url: None,
            api: None,
        },
    );
    let mut web = websearch_instance("exa");
    web.secret_ref = Some("ref-web".into());
    profile.websearch.insert("exa".into(), web);
    let mut decision = decision_settings(&[("jev", "jev")]);
    decision.instances.get_mut("jev").unwrap().secret_ref = Some("ref-decision".into());
    profile.decision = Some(decision);
    settings.profiles.insert("personal".into(), profile);

    let legacy = LegacyMap::new();
    legacy.values.set("ref-provider", "sk-provider").unwrap();
    legacy.values.set("ref-web", "sk-web").unwrap();
    legacy.values.set("ref-decision", "sk-decision").unwrap();

    let store = VolatileSecretStore::new();
    store.set("ref-web", "already-there").unwrap();
    let before = settings.clone();
    assert_eq!(settings.import_legacy_secrets(&legacy, &store), 2);
    assert_eq!(settings, before, "the import never rewrites the config");
    assert_eq!(
        store.get("ref-provider").unwrap().as_deref(),
        Some("sk-provider")
    );
    assert_eq!(
        store.get("ref-web").unwrap().as_deref(),
        Some("already-there"),
        "an existing file-store value wins"
    );
    assert_eq!(
        store.get("ref-decision").unwrap().as_deref(),
        Some("sk-decision")
    );
    assert_eq!(store.get("ref-missing").unwrap(), None);
    // Idempotent: the file store has them now.
    assert_eq!(settings.import_legacy_secrets(&legacy, &store), 0);
}

#[test]
fn an_unreachable_old_keyring_leaves_every_ref_alone() {
    let mut settings = Settings::default();
    settings.set_for_profile(Some("personal"), |profile| {
        profile.instances.insert(
            "google".into(),
            ProviderInstance {
                provider: "google".into(),
                api_key: None,
                secret_ref: Some("ref-1".into()),
                base_url: None,
                api: None,
            },
        );
    });
    let store = VolatileSecretStore::new();
    assert_eq!(
        settings.import_legacy_secrets(&KeyringSecretReader::disabled(), &store),
        0
    );
    assert_eq!(store.get("ref-1").unwrap(), None);
}

#[test]
fn a_ref_the_file_store_has_never_reaches_the_keyring() {
    let mut settings = Settings::default();
    settings.set_for_profile(Some("personal"), |profile| {
        for (name, id) in [("google", "ref-present"), ("openai", "ref-missing")] {
            profile.instances.insert(
                name.into(),
                ProviderInstance {
                    provider: name.into(),
                    api_key: None,
                    secret_ref: Some(id.into()),
                    base_url: None,
                    api: None,
                },
            );
        }
    });
    let legacy = LegacyMap::new();
    legacy.values.set("ref-present", "old").unwrap();
    legacy.values.set("ref-missing", "old").unwrap();
    let store = VolatileSecretStore::new();
    store.set("ref-present", "new").unwrap();
    assert_eq!(settings.import_legacy_secrets(&legacy, &store), 1);
    assert_eq!(legacy.calls(), 1, "only the missing ref asked the keyring");
    assert_eq!(
        store.get("ref-present").unwrap().as_deref(),
        Some("new"),
        "the file-store value wins"
    );
}
