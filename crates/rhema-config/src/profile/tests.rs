use super::*;

use crate::instances::tests::{UnreachableStore, decision_settings, websearch_instance};
use crate::secrets::VolatileSecretStore;
use crate::settings::Settings;

#[test]
fn debug_prints_presence_never_the_key() {
    let profile = Profile {
        provider: Some("google".into()),
        instances: BTreeMap::from([(
            "main".into(),
            ProviderInstance {
                provider: "google".into(),
                api_key: Some("sk-live-secret".into()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        )]),
        sandbox: Some(SandboxSettings {
            kind: Some("http".into()),
            token: Some("bearer-secret".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let printed = format!("{profile:?}");
    assert!(!printed.contains("sk-live-secret"), "{printed}");
    assert!(!printed.contains("bearer-secret"), "{printed}");
    assert!(printed.contains("Some(\"[…]\")"), "{printed}");
}

#[test]
fn anthropic_and_openai_profiles_need_their_own_key_to_serve() {
    let keyed = |provider: &str| {
        let mut profile = Profile {
            provider: Some(provider.to_owned()),
            ..Profile::default()
        };
        profile.upsert_instance(
            provider,
            ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some("k".to_owned()),
                secret_ref: None,
                base_url: None,
                api: None,
            },
        );
        profile
    };
    // A stored key makes the profile usable...
    assert!(keyed("anthropic").has_credentials(None));
    assert!(keyed("openai").has_credentials(None));
    assert!(keyed("deepseek").has_credentials(None));
    assert!(keyed("copilot").has_credentials(None));
    assert!(keyed("chatgpt").has_credentials(None));
    assert!(keyed("mistral").has_credentials(None));
    assert!(keyed("mistral-vibe").has_credentials(None));
    // ...and each provider needs exactly its own key.
    assert!(
        keyed("anthropic")
            .stored_provider_key("openai", None)
            .is_none()
    );
    assert!(
        keyed("openai")
            .stored_provider_key("anthropic", None)
            .is_none()
    );
    // No stored key and no env var: not usable. (Machines running the
    // suite with these env vars exported would see the env key win,
    // per the convention above.)
    let bare = |provider: &str| Profile {
        provider: Some(provider.to_owned()),
        ..Profile::default()
    };
    assert!(!bare("anthropic").has_credentials(None));
    assert!(!bare("openai").has_credentials(None));
    assert!(!bare("deepseek").has_credentials(None));
    assert!(!bare("copilot").has_credentials(None));
    assert!(!bare("chatgpt").has_credentials(None));
    assert!(!bare("mistral").has_credentials(None));
    assert!(!bare("mistral-vibe").has_credentials(None));
    assert!(
        !bare("mystery").has_credentials(None),
        "an unknown provider never reads as google"
    );
}

#[test]
fn the_provider_name_follows_the_same_env_wins_convention() {
    // A blank RHEMA_PROVIDER counts as unset: the stored provider
    // serves. The real env is only read when the suite does not
    // export the var itself; the pure resolver carries the rest of
    // the env-wins coverage (a test setting it would race the run).
    let blank = |name: &str| (name == "RHEMA_PROVIDER").then(String::new);
    assert_eq!(
        resolve_key(blank, &["RHEMA_PROVIDER"], Some("openrouter".into())),
        Some("openrouter".into())
    );
    assert_eq!(
        resolve_key(
            |_| Some("openrouter".into()),
            &["RHEMA_PROVIDER"],
            Some("google".into())
        ),
        Some("openrouter".into())
    );
    assert_eq!(resolve_key(|_| None, &["RHEMA_PROVIDER"], None), None);
    if std::env::var_os("RHEMA_PROVIDER").is_none() {
        let profile = Profile {
            provider: Some("openrouter".into()),
            ..Profile::default()
        };
        assert_eq!(profile.provider_name().as_deref(), Some("openrouter"));
        assert_eq!(Profile::default().provider_name(), None);
    }
}

#[test]
fn the_sandbox_instance_resolves_through_the_active_name_then_the_first() {
    let mut profile = Profile {
        sandbox: Some(SandboxSettings {
            kind: Some("lxd".into()),
            active: Some("dev-vm".into()),
            ..SandboxSettings::default()
        }),
        ..Profile::default()
    };
    profile.place_sandbox_instance(
        "dev-vm",
        SandboxInstance {
            kind: "lxd".into(),
            target: Some("dev".into()),
            vm: Some(true),
        },
    );
    profile.place_sandbox_instance(
        "build",
        SandboxInstance {
            kind: "lxd".into(),
            target: Some("build".into()),
            vm: None,
        },
    );
    // The active pointer names the one.
    let (name, instance) = profile.sandbox_instance_for("lxd").unwrap();
    assert_eq!(name, "dev-vm");
    assert_eq!(instance.target.as_deref(), Some("dev"));
    // A pointer at a different kind falls through to the first of
    // the kind asked for.
    profile.sandbox.as_mut().unwrap().active = Some("build".into());
    let (name, instance) = profile.sandbox_instance_for("lxd").unwrap();
    assert_eq!(name, "build", "the pointer names an lxd instance");
    assert_eq!(instance.target.as_deref(), Some("build"));
    profile.sandbox.as_mut().unwrap().active = Some("gone".into());
    let (name, _) = profile.sandbox_instance_for("lxd").unwrap();
    assert_eq!(name, "build", "a dangling pointer falls through");
    // No instance of the kind: the section's facts are the
    // caller's fallback.
    assert_eq!(profile.sandbox_instance_for("docker"), None);
}

#[test]
fn removing_a_sandbox_instance_clears_the_active_pointer() {
    let mut profile = Profile {
        sandbox: Some(SandboxSettings {
            kind: Some("lxd".into()),
            active: Some("dev-vm".into()),
            ..SandboxSettings::default()
        }),
        ..Profile::default()
    };
    profile.place_sandbox_instance(
        "dev-vm",
        SandboxInstance {
            kind: "lxd".into(),
            target: Some("dev".into()),
            vm: None,
        },
    );
    profile.place_sandbox_instance(
        "build",
        SandboxInstance {
            kind: "lxd".into(),
            target: Some("build".into()),
            vm: None,
        },
    );
    let removed = profile.remove_sandbox_instance("dev-vm").unwrap();
    assert_eq!(removed.target.as_deref(), Some("dev"));
    assert_eq!(
        profile.sandbox.as_ref().unwrap().active,
        None,
        "the pointer went with it"
    );
    assert_eq!(profile.sandbox_instance_for("lxd").unwrap().0, "build");
    // Removing a name that is not there removes nothing.
    assert!(profile.remove_sandbox_instance("dev-vm").is_none());
}

#[test]
fn the_active_instance_resolves_through_instance_name_then_provider() {
    let profile = Profile {
        provider: Some("llama-cpp".into()),
        instance: Some("gpu box".into()),
        instances: BTreeMap::from([
            (
                "gpu box".to_owned(),
                ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://gpu:8080".into()),
                    api: None,
                },
            ),
            (
                "home".to_owned(),
                ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                    api: None,
                },
            ),
        ]),
        ..Profile::default()
    };
    let (name, instance) = profile.active_instance().unwrap();
    assert_eq!(name, "gpu box");
    assert_eq!(
        profile.instance_url(instance).as_deref(),
        Some("http://gpu:8080")
    );
    // A dangling `instance` name falls back to the first instance of
    // the active provider.
    let mut dangling = profile.clone();
    dangling.instance = Some("nope".into());
    assert_eq!(dangling.active_instance().unwrap().0, "gpu box");
    // The same provider with a different active provider set: the
    // first stored llama instance serves.
    let mut elsewhere = profile.clone();
    elsewhere.provider = Some("openai".into());
    elsewhere.instance = Some("gpu box".into());
    assert_eq!(elsewhere.active_instance(), None);
    assert_eq!(
        elsewhere.instance_for_provider("llama-cpp").unwrap().0,
        "gpu box"
    );

    // Removing the active instance clears the pointer, not the
    // provider.
    let mut removed = profile.clone();
    assert!(removed.remove_instance("gpu box").is_some());
    assert_eq!(removed.instance, None);
    assert_eq!(removed.provider.as_deref(), Some("llama-cpp"));
    assert_eq!(removed.active_instance().unwrap().0, "home");
    // Removing a name that is not there removes nothing.
    assert!(removed.remove_instance("gpu box").is_none());
}

#[test]
fn two_instances_of_one_provider_hold_their_own_keys() {
    let mut profile = Profile {
        provider: Some("openai".into()),
        instance: Some("personal".into()),
        ..Profile::default()
    };
    profile.upsert_instance(
        "personal",
        ProviderInstance {
            provider: "openai".into(),
            api_key: Some("sk-personal".into()),
            secret_ref: None,
            base_url: None,
            api: None,
        },
    );
    profile.upsert_instance(
        "proxy",
        ProviderInstance {
            provider: "openai".into(),
            api_key: Some("sk-proxy".into()),
            secret_ref: None,
            base_url: Some("http://localhost:1234/v1".into()),
            api: None,
        },
    );
    assert_eq!(
        profile.api_key("openai", None).as_deref(),
        Some("sk-personal")
    );
    assert_eq!(
        profile.openai_base_url().as_deref(),
        Some("https://api.openai.com/v1")
    );
    // Activating the proxy instance moves both the key and the base.
    profile.instance = Some("proxy".into());
    assert_eq!(profile.api_key("openai", None).as_deref(), Some("sk-proxy"));
    assert_eq!(
        profile.openai_base_url().as_deref(),
        Some("http://localhost:1234/v1")
    );
    assert!(profile.has_credentials(None));
    // Pointing at a missing name falls back to the first openai
    // instance rather than failing open.
    profile.instance = Some("nope".into());
    assert_eq!(
        profile.api_key("openai", None).as_deref(),
        Some("sk-personal")
    );

    // The env-free read serves callers with their own precedence.
    profile.instance = Some("proxy".into());
    assert_eq!(
        profile.stored_provider_key("openai", None).as_deref(),
        Some("sk-proxy")
    );
}

#[test]
fn a_zai_instance_honors_its_stored_base_url() {
    let mut profile = Profile {
        provider: Some("zai".into()),
        instance: Some("plan".into()),
        ..Profile::default()
    };
    profile.upsert_instance(
        "plan",
        ProviderInstance {
            provider: "zai".into(),
            api_key: Some("zkey".into()),
            secret_ref: None,
            base_url: Some("https://proxy.example/api/anthropic".into()),
            api: None,
        },
    );
    assert_eq!(profile.api_key("zai", None).as_deref(), Some("zkey"));
    assert_eq!(
        profile.instance_url_for("zai").as_deref(),
        Some("https://proxy.example/api/anthropic")
    );
    assert!(profile.has_credentials(None));
    // A blank stored base reads as unset: the caller defaults the
    // route, the same convention the openai kind follows.
    profile.upsert_instance(
        "plan",
        ProviderInstance {
            provider: "zai".into(),
            api_key: Some("zkey".into()),
            secret_ref: None,
            base_url: Some("   ".into()),
            api: None,
        },
    );
    assert_eq!(profile.instance_url_for("zai"), None);
}

#[test]
fn an_opencode_instance_honors_its_stored_route() {
    let mut profile = Profile {
        provider: Some("opencode".into()),
        instance: Some("OpenCode Go".into()),
        ..Profile::default()
    };
    profile.upsert_instance(
        "OpenCode Go",
        ProviderInstance {
            provider: "opencode".into(),
            api_key: Some("ockey".into()),
            secret_ref: None,
            base_url: Some("https://opencode.ai/inference/go/openai/v1".into()),
            api: None,
        },
    );
    assert_eq!(profile.api_key("opencode", None).as_deref(), Some("ockey"));
    assert_eq!(
        profile.instance_url_for("opencode").as_deref(),
        Some("https://opencode.ai/inference/go/openai/v1"),
        "the Go route must survive every rebuild from the profile"
    );
}

/// The store tier serves before the literal, an unreachable
/// store falls through to it, and the environment tier is covered
/// by the pure resolver tests above.
#[test]
fn the_store_tier_serves_before_the_literal() {
    use crate::secrets::VolatileSecretStore;
    let mut profile = Profile::default();
    profile.upsert_instance(
        "anthropic",
        ProviderInstance {
            provider: "anthropic".into(),
            api_key: Some("file-key".into()),
            secret_ref: Some("ref-1".into()),
            base_url: None,
            api: None,
        },
    );
    let key_of = |profile: &Profile, secrets: Option<&dyn SecretStore>| {
        profile.stored_instance_key(
            profile.instance_for_provider("anthropic").unwrap().1,
            secrets,
        )
    };
    let store = VolatileSecretStore::new();
    // No entry yet: the literal serves.
    assert_eq!(key_of(&profile, Some(&store)).as_deref(), Some("file-key"));
    // The entry lands: the store outranks the literal.
    store.set("ref-1", "chain-key").unwrap();
    assert_eq!(key_of(&profile, Some(&store)).as_deref(), Some("chain-key"));
    // An unreachable store reads as "not in the store": the literal
    // serves, nothing is lost.
    assert_eq!(
        key_of(&profile, Some(&UnreachableStore)).as_deref(),
        Some("file-key")
    );
    // No store at all, the pre-store callers, same answer.
    assert_eq!(key_of(&profile, None).as_deref(), Some("file-key"));
}

#[test]
fn place_key_prefers_the_chain_and_falls_back_to_the_file() {
    use crate::secrets::VolatileSecretStore;
    let mut profile = Profile {
        provider: Some("anthropic".into()),
        ..Profile::default()
    };
    profile.upsert_instance(
        "anthropic",
        ProviderInstance {
            provider: "anthropic".into(),
            api_key: None,
            secret_ref: None,
            base_url: None,
            api: None,
        },
    );
    // With a working store the key lives in the chain and the
    // instance carries only the ref.
    let store = VolatileSecretStore::new();
    assert_eq!(
        profile.place_key("anthropic", " sk-new ", Some(&store)),
        Some(KeyLocation::Store)
    );
    let instance = profile.instance_for_provider("anthropic").unwrap().1;
    assert_eq!(instance.api_key, None);
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("sk-new"));
    assert_eq!(
        profile
            .stored_provider_key("anthropic", Some(&store))
            .as_deref(),
        Some("sk-new")
    );

    // Without a store the file keeps the key, trimmed.
    assert_eq!(
        profile.place_key("anthropic", " sk-file ", None),
        Some(KeyLocation::File)
    );
    let instance = profile.instance_for_provider("anthropic").unwrap().1;
    assert_eq!(instance.api_key.as_deref(), Some("sk-file"));

    // An unreachable store lands the key in the file too.
    assert_eq!(
        profile.place_key("anthropic", "sk-again", Some(&UnreachableStore)),
        Some(KeyLocation::File)
    );
    assert_eq!(
        profile
            .instance_for_provider("anthropic")
            .unwrap()
            .1
            .api_key
            .as_deref(),
        Some("sk-again")
    );
    // An unknown instance name places nothing.
    assert_eq!(profile.place_key("nope", "k", Some(&store)), None);
}

#[test]
fn clearing_a_key_drops_its_stale_store_entry() {
    use crate::secrets::VolatileSecretStore;
    let mut profile = Profile::default();
    profile.upsert_instance(
        "anthropic",
        ProviderInstance {
            provider: "anthropic".into(),
            api_key: None,
            secret_ref: None,
            base_url: None,
            api: None,
        },
    );
    let store = VolatileSecretStore::new();
    profile.place_key("anthropic", "old", Some(&store));
    let instance = profile.instance_for_provider("anthropic").unwrap().1;
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("old"));

    assert_eq!(
        profile.place_key("anthropic", "   ", Some(&store)),
        Some(KeyLocation::File)
    );
    let instance = profile.instance_for_provider("anthropic").unwrap().1;
    assert_eq!(instance.api_key, None);
    assert_eq!(instance.secret_ref, None);
    assert_eq!(store.get(&id).unwrap(), None, "the entry is gone");
}

#[test]
fn a_file_fallback_drops_a_stale_store_ref() {
    use crate::secrets::VolatileSecretStore;
    let mut profile = Profile::default();
    profile.upsert_instance(
        "anthropic",
        ProviderInstance {
            provider: "anthropic".into(),
            api_key: None,
            secret_ref: Some("old-ref".into()),
            base_url: None,
            api: None,
        },
    );
    let store = VolatileSecretStore::new();
    store.set("old-ref", "old").unwrap();

    assert_eq!(
        profile.place_key("anthropic", "new", Some(&UnreachableStore)),
        Some(KeyLocation::File)
    );
    let instance = profile.instance_for_provider("anthropic").unwrap().1;
    assert_eq!(instance.api_key.as_deref(), Some("new"));
    assert_eq!(instance.secret_ref, None);
}

/// A profile carrying one sandbox section, and nothing else.
#[test]
fn ecrou_persist_defaults_on_and_stores_an_explicit_off() {
    let profile = sandboxed_profile(SandboxSettings::default());
    assert!(profile.sandbox_persist(), "unset keeps rootfs state");

    let settings = SandboxSettings {
        persist: Some(false),
        ..SandboxSettings::default()
    };
    assert!(!sandboxed_profile(settings).sandbox_persist());
}

fn sandboxed_profile(sandbox: SandboxSettings) -> Profile {
    Profile {
        sandbox: Some(sandbox),
        ..Profile::default()
    }
}

#[test]
fn a_profile_round_trips_with_a_full_sandbox_section() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let config = Settings {
        profiles: BTreeMap::from([(
            "personal".to_owned(),
            sandboxed_profile(SandboxSettings {
                kind: Some("http".into()),
                url: Some("https://sandbox.internal:7331".into()),
                token: Some("literal-token".into()),
                token_ref: Some("ref-sandbox".into()),
                host: Some("deploy@box".into()),
                container: Some("rhema-dev".into()),
                active: Some("dev-vm".into()),
                instance: Some("dev-vm".into()),
                vm: Some(true),
                image: Some("alpine:3.23".into()),
                persist: None,
                inject_credentials: false,
            }),
        )]),
        ..Settings::default()
    };
    Settings::save_to(&path, &config).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    for key in [
        "sandbox:",
        "kind: http",
        "url:",
        "token:",
        "token_ref:",
        "host:",
        "container:",
    ] {
        assert!(raw.contains(key), "missing `{key}` in yaml on disk: {raw}");
    }
    let loaded = Settings::load_from(&path).unwrap();
    assert_eq!(loaded, config);

    // A typo'd sandbox key is loud, the file-wide strictness.
    std::fs::write(
        &path,
        "profiles:\n  personal:\n    sandbox:\n      knd: docker\n",
    )
    .unwrap();
    let err = Settings::load_from(&path).expect_err("a typo'd sandbox key must not read as unset");
    assert!(err.to_string().contains("knd"), "names the key: {err}");
}

#[test]
fn the_sandbox_fields_read_back_through_the_profile() {
    // The public methods reach the real environment only when the
    // suite does not export the vars itself; the pure resolver
    // test above carries the env-wins coverage.
    if ["RHEMA_SANDBOX", "RHEMA_SANDBOX_URL", "RHEMA_SANDBOX_TOKEN"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    let profile = sandboxed_profile(SandboxSettings {
        kind: Some(" ssh ".into()),
        url: Some(" http://box:7331 ".into()),
        token: Some(" literal ".into()),
        token_ref: Some("ref-sandbox".into()),
        host: Some(" deploy@box ".into()),
        container: Some(" rhema-dev ".into()),
        active: None,
        instance: Some(" dev-vm ".into()),
        vm: Some(true),
        image: Some(" alpine:3.23 ".into()),
        persist: None,
        inject_credentials: false,
    });
    assert_eq!(profile.sandbox_kind().as_deref(), Some("ssh"));
    assert_eq!(profile.sandbox_url().as_deref(), Some("http://box:7331"));
    assert_eq!(profile.sandbox_token(None).as_deref(), Some("literal"));
    assert_eq!(profile.sandbox_image().as_deref(), Some("alpine:3.23"));
    assert_eq!(profile.sandbox_host().as_deref(), Some("deploy@box"));
    assert_eq!(profile.sandbox_container().as_deref(), Some("rhema-dev"));

    // A profile without the section resolves to nothing anywhere.
    let bare = Profile::default();
    assert_eq!(bare.sandbox_kind(), None);
    assert_eq!(bare.sandbox_url(), None);
    assert_eq!(bare.sandbox_token(None), None);
    assert_eq!(bare.sandbox_host(), None);
    assert_eq!(bare.sandbox_container(), None);

    // Blank stored values count as unset, host and container too,
    // which have no env tier to fall through to.
    let blank = sandboxed_profile(SandboxSettings {
        kind: Some("  ".into()),
        url: Some(" ".into()),
        host: Some("  ".into()),
        container: Some("".into()),
        ..SandboxSettings::default()
    });
    assert_eq!(blank.sandbox_kind(), None);
    assert_eq!(blank.sandbox_url(), None);
    assert_eq!(blank.sandbox_host(), None);
    assert_eq!(blank.sandbox_container(), None);
}

#[test]
fn the_sandbox_token_tiers_run_store_then_literal() {
    use crate::secrets::VolatileSecretStore;
    let profile = sandboxed_profile(SandboxSettings {
        kind: Some("http".into()),
        url: Some("https://box:7331".into()),
        token: Some("file-token".into()),
        token_ref: Some("ref-sandbox".into()),
        ..SandboxSettings::default()
    });
    let store = VolatileSecretStore::new();
    // No entry yet: the literal serves.
    assert_eq!(
        profile.stored_sandbox_token(Some(&store)).as_deref(),
        Some("file-token")
    );
    // The entry lands: the store outranks the literal. (The env
    // tier outranks both, the resolver test above.)
    store.set("ref-sandbox", "chain-token").unwrap();
    assert_eq!(
        profile.stored_sandbox_token(Some(&store)).as_deref(),
        Some("chain-token")
    );
    // An unreachable store reads as "not in the store": the
    // literal serves, nothing is lost.
    assert_eq!(
        profile
            .stored_sandbox_token(Some(&UnreachableStore))
            .as_deref(),
        Some("file-token")
    );
    // A blank store entry counts as unset: the literal serves.
    store.set("ref-sandbox", "  ").unwrap();
    assert_eq!(
        profile.stored_sandbox_token(Some(&store)).as_deref(),
        Some("file-token")
    );
    // No store at all, the pre-store callers, same answer.
    assert_eq!(
        profile.stored_sandbox_token(None).as_deref(),
        Some("file-token")
    );
    // No section, no token, whatever the store holds.
    store.set("ref-sandbox", "chain-token").unwrap();
    assert_eq!(Profile::default().stored_sandbox_token(Some(&store)), None);
}

#[test]
fn the_websearch_chain_walks_the_order_and_skips_unknown_names() {
    let mut profile = Profile::default();
    profile
        .websearch
        .insert("brave-main".into(), websearch_instance("brave"));
    profile
        .websearch
        .insert("exa-work".into(), websearch_instance("exa"));
    profile.websearch_order = vec!["exa-work".into(), "ghost".into(), "brave-main".into()];
    let chain = profile.websearch_chain();
    let names: Vec<_> = chain.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["exa-work", "brave-main"]);
}

#[test]
fn an_instance_missing_from_the_order_is_inert() {
    let mut profile = Profile::default();
    profile
        .websearch
        .insert("brave-main".into(), websearch_instance("brave"));
    assert!(profile.websearch_chain().is_empty());
}

#[test]
fn place_websearch_key_prefers_the_chain_and_falls_back_to_the_file() {
    let mut profile = Profile::default();
    profile
        .websearch
        .insert("brave-main".into(), websearch_instance("brave"));
    let store = VolatileSecretStore::new();
    assert_eq!(
        profile.place_websearch_key("brave-main", " sk-new ", Some(&store)),
        Some(KeyLocation::Store)
    );
    let instance = &profile.websearch["brave-main"];
    assert_eq!(instance.api_key, None);
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("sk-new"));
    // No store, no working store: the file keeps the key.
    assert_eq!(
        profile.place_websearch_key("brave-main", " sk-file ", None),
        Some(KeyLocation::File)
    );
    assert_eq!(
        profile.websearch["brave-main"].api_key.as_deref(),
        Some("sk-file")
    );
    // A name bearing no instance is None, not a silent insert.
    assert_eq!(
        profile.place_websearch_key("ghost", "k", Some(&store)),
        None
    );
}

#[test]
fn the_default_instance_is_the_named_one_else_the_first() {
    let mut profile = Profile::default();
    assert!(profile.active_decision_instance().is_none(), "none stored");
    let entries = [("clef-work", "clef"), ("jev-main", "jev")];
    profile.decision = Some(decision_settings(&entries));
    // Unset: the first instance, by name.
    let (name, _) = profile.active_decision_instance().unwrap();
    assert_eq!(name, "clef-work");
    // Named: that one, whichever the order says.
    profile.decision.as_mut().unwrap().instance = Some("jev-main".into());
    let (name, _) = profile.active_decision_instance().unwrap();
    assert_eq!(name, "jev-main");
    // Dangling: does not count, the first again.
    profile.decision.as_mut().unwrap().instance = Some("ghost".into());
    let (name, _) = profile.active_decision_instance().unwrap();
    assert_eq!(name, "clef-work");
    // An empty section is no instance: the environment tier.
    profile.decision = Some(DecisionSettings::default());
    assert!(profile.active_decision_instance().is_none());
}

#[test]
fn the_profile_file_carries_the_section_only_when_filled() {
    let mut profile = Profile::default();
    let bare = serde_yaml::to_string(&profile).unwrap();
    assert!(!bare.contains("decision"), "{bare}");
    profile.decision = Some(decision_settings(&[("jev-main", "jev")]));
    let text = serde_yaml::to_string(&profile).unwrap();
    assert!(text.contains("decision:\n"), "{text}");
    assert!(text.contains("  instances:\n"), "{text}");
    assert!(text.contains("    jev-main:\n"), "{text}");
    assert!(!text.contains("\n  instance:"), "unset: {text}");
    profile.decision.as_mut().unwrap().instance = Some("jev-main".into());
    let text = serde_yaml::to_string(&profile).unwrap();
    assert!(text.contains("  instance: jev-main\n"), "{text}");
    let back: Profile = serde_yaml::from_str(&text).unwrap();
    assert_eq!(back.decision, profile.decision);
    // A nulled section is an absent one.
    let nulled: Profile = serde_yaml::from_str("decision: null\n").unwrap();
    assert!(nulled.decision.is_none());
    // The section is strict: an entry's key spelled at the
    // section level, or a section key inside an entry, is loud.
    let err = serde_yaml::from_str::<Profile>("decision:\n  api_key: k\n").unwrap_err();
    assert!(format!("{err}").contains("unknown field"), "{err}");
    let err =
        serde_yaml::from_str::<Profile>("decision:\n  instances:\n    jev:\n      instance: x\n")
            .unwrap_err();
    assert!(format!("{err}").contains("unknown field"), "{err}");
}

#[test]
fn place_decision_key_prefers_the_store_and_falls_back_to_the_file() {
    let mut profile = Profile::default();
    assert_eq!(
        profile.place_decision_key("jev", "k", None),
        None,
        "no section"
    );
    profile.decision = Some(decision_settings(&[("jev", "jev")]));
    assert_eq!(profile.place_decision_key("ghost", "k", None), None);
    let store = VolatileSecretStore::new();
    assert_eq!(
        profile.place_decision_key("jev", " sk-new ", Some(&store)),
        Some(KeyLocation::Store)
    );
    let instance = &profile.decision.as_ref().unwrap().instances["jev"];
    assert_eq!(instance.api_key, None);
    let id = instance.secret_ref.clone().unwrap();
    assert_eq!(store.get(&id).unwrap().as_deref(), Some("sk-new"));
    assert_eq!(
        profile.place_decision_key("jev", " sk-file ", None),
        Some(KeyLocation::File)
    );
    assert_eq!(
        profile.decision.as_ref().unwrap().instances["jev"]
            .api_key
            .as_deref(),
        Some("sk-file")
    );
}

#[test]
fn an_old_host_mcp_map_loads_once_and_never_serializes_again() {
    let yaml = "profiles:\n  personal:\n    mcp_servers:\n      jira:\n        description: Jira\n        command: [uvx, jira-mcp]\n";
    let settings: Settings = serde_yaml::from_str(yaml).expect("the old shape still loads");
    assert_eq!(settings.profiles["personal"].legacy_mcp_servers.len(), 1);
    let round = serde_yaml::to_string(&settings).unwrap();
    assert!(!round.contains("mcp_servers"), "{round}");
}
