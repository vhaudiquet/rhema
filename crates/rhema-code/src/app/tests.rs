//! The app's behavior tests: the `impl App` slices live beside the
//! code they exercise, the tests share them from here.
use super::*;
    use super::*;
    use crate::approval::TuiApproval;
    use crate::events::OwnedEvent;
    use crate::screens::LoginProvider;
    use crate::store::MemoryStore;
    use crate::transcript::Line;
    use rhema_coding_agent::{Approval, CodingAgentConfig};
    use rhema_config::secrets::SecretStore as _;
    use rhema_core::{Message, Role, Session, StopReason, ToolCall, Usage};
    use rhema_provider::ModelCard;
    use rhema_provider_google::GoogleProvider;
    use std::collections::BTreeMap;

    /// A described skill folder under `dir`, the way discovery reads
    /// one.
    fn write_skill(dir: &Path, name: &str, description: &str) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("SKILL.md");
        std::fs::write(
            &file,
            format!("---\ndescription: {description}\n---\n\nBody.\n"),
        )
        .unwrap();
        file
    }

    #[test]
    fn skill_rows_name_their_source_and_the_rhema_copy_hides_the_agents_one() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let agents = home.path().join(".agents").join("skills");
        let global = home.path().join(".config").join("rhema").join("skills");
        write_skill(&agents, "borrowed", "the agents trigger");
        // A rhema-global copy and an .agents copy share one name. The
        // page shows one row, tagged global, and the hidden copy never
        // lists.
        write_skill(&agents, "shared", "agents trigger");
        write_skill(&global, "shared", "global trigger");
        write_skill(
            &workspace.path().join(".rhema").join("skills"),
            "local",
            "workspace trigger",
        );

        let disabled = std::collections::BTreeSet::new();
        let rows = skill_rows(Some(&agents), Some(&global), workspace.path(), &disabled);
        let tagged = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .unwrap_or_else(|| panic!("no {name} row in {rows:?}"))
                .source
        };
        assert_eq!(rows.len(), 3, "the shared .agents copy hides: {rows:?}");
        assert_eq!(tagged("local"), SkillSource::Workspace);
        assert_eq!(tagged("shared"), SkillSource::Global);
        assert_eq!(tagged("borrowed"), SkillSource::Agents);
    }

    #[test]
    fn a_workspace_skill_of_an_agents_name_shadows_to_workspace() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let agents = home.path().join(".agents").join("skills");
        write_skill(&agents, "shared", "agents trigger");
        write_skill(
            &workspace.path().join(".rhema").join("skills"),
            "shared",
            "workspace trigger",
        );

        let rows = skill_rows(
            Some(&agents),
            None,
            workspace.path(),
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, SkillSource::Workspace);
        assert_eq!(rows[0].description, "workspace trigger");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn model(id: &str) -> ModelCard {
        ModelCard {
            id: id.into(),
            display_name: id.into(),
            description: String::new(),
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
            supports_vision: None,
            generation_methods: vec!["generateContent".into()],
            pricing: None,
        }
    }

    fn models() -> Vec<ModelCard> {
        vec![model("m-b"), model("m-a")]
    }

    /// A `ModelsLoaded` result of one provider's catalog, the shape the
    /// fetch task reports per queried backend. The instance is named
    /// after the provider, the way a migrated login lands.
    fn loaded(provider: &'static str, models: Vec<ModelCard>) -> Vec<crate::events::CatalogFetch> {
        vec![crate::events::CatalogFetch {
            provider,
            instance: Some(provider.to_owned()),
            result: Ok(models),
        }]
    }

    /// A `ModelsLoaded` result of one provider's failure.
    fn failed(provider: &'static str, message: &str) -> Vec<crate::events::CatalogFetch> {
        vec![crate::events::CatalogFetch {
            provider,
            instance: Some(provider.to_owned()),
            result: Err(message.to_owned()),
        }]
    }

    /// A priced card, for cost-math tests: the price rides the row, the
    /// way the adapters hand catalogs out.
    fn priced_model(id: &str, pricing: rhema_provider::Pricing) -> ModelCard {
        ModelCard {
            pricing: Some(pricing),
            ..model(id)
        }
    }

    /// A llama.cpp row: an id with none of the metadata Google reports.
    fn llama_model(id: &str) -> ModelCard {
        ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            supports_vision: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
        }
    }

    fn test_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "ls" }),
            provider_data: serde_json::Value::Null,
        }
    }

    struct Harness {
        app: App,
        store: Arc<MemoryStore>,
        /// The keychain the app places logins in, when the test gave it
        /// one.
        secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
    }

    /// Stores `key` as `provider`'s instance on `profile` — the login
    /// flow's write shape.
    fn store_key(profile: &mut rhema_config::Profile, provider: &str, key: &str) {
        profile.upsert_instance(
            provider,
            rhema_config::ProviderInstance {
                provider: provider.to_owned(),
                api_key: Some(key.to_owned()),
                secret_ref: None,
                base_url: None,
            },
        );
    }

    /// A profile serving turns on `provider` through one stored instance
    /// named after it.
    fn keyed_profile(provider: &str, key: &str) -> rhema_config::Profile {
        let mut profile = rhema_config::Profile {
            provider: Some(provider.to_owned()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut profile, provider, key);
        profile
    }

    fn harness(api_key: Option<&str>, model: Option<&str>, session: Session) -> Harness {
        harness_inner(api_key, model, session, None)
    }

    /// A harness whose app places logins in a volatile keychain.
    fn harness_with_secrets(
        api_key: Option<&str>,
        model: Option<&str>,
        session: Session,
    ) -> Harness {
        harness_inner(
            api_key,
            model,
            session,
            Some(Arc::new(rhema_config::secrets::VolatileSecretStore::new())),
        )
    }

    fn harness_inner(
        api_key: Option<&str>,
        model: Option<&str>,
        session: Session,
        secrets: Option<Arc<rhema_config::secrets::VolatileSecretStore>>,
    ) -> Harness {
        let mut personal = rhema_config::Profile {
            // The key alone never served a turn: a usable
            // profile names its provider too.
            provider: Some("google".into()),
            model: model.map(str::to_owned),
            ..rhema_config::Profile::default()
        };
        if let Some(key) = api_key {
            personal.upsert_instance(
                "google",
                rhema_config::ProviderInstance {
                    provider: "google".into(),
                    api_key: Some(key.to_owned()),
                    secret_ref: None,
                    base_url: None,
                },
            );
        }
        let config = Config {
            profiles: BTreeMap::from([("personal".to_owned(), personal)]),
            ..Default::default()
        };
        let store = Arc::new(MemoryStore::new(config.clone()));
        let backend: Option<Backend> = api_key.map(|key| Backend::Google {
            api_key: key.to_owned(),
        });
        let app = App::new(AppSetup {
            store: store.clone(),
            checkpoint: None,
            config,
            backend,
            session,
            secrets: secrets
                .clone()
                .map(|store| store as Arc<dyn rhema_config::secrets::SecretStore>),
            sandboxed: None,
            sandbox_machines: crate::sandbox::SandboxMachines::default(),
            home: Some(std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id()))),
            onboard: false,
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: crate::palette::ThemeKind::Dark,
                m3_source: None,
                custom: None,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some("personal".into()),
                workspace: PathBuf::from("."),
                max_steps: 4,
                no_transcript: false,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        Harness {
            app,
            store,
            secrets,
        }
    }

    /// Drives the settings' add-instance dialog to the credential stage
    /// for a provider sitting at `row` on the provider list, accepting
    /// the suggested instance name — ready to type the credential. The
    /// settings must be open (or `/login` dispatchable) first.
    fn descend_to_credential(h: &mut Harness, row: usize) -> String {
        use crate::screens::DialogStage;
        if !matches!(h.app.screen, Screen::Settings(_)) {
            crate::commands::dispatch(&mut h.app, "/login");
        }
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        // Down to the add row of the instance list, then enter.
        for _ in 0..settings.instance_rows().len().saturating_sub(1) {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        for _ in 0..row {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        let dialog = settings.dialog.as_ref().expect("the add dialog");
        let name = dialog.input.clone();
        assert_eq!(dialog.stage, DialogStage::NameInstance);
        // Enter accepts the suggested name; copilot's flow starts
        // instead of asking for a key, which its own tests drive.
        h.app.on_key(key(KeyCode::Enter));
        name
    }

    #[test]
    fn a_paste_on_the_add_dialog_fills_the_credential() {
        // The chat is the opening screen now; /login opens the page
        // the add dialog lives on.
        let mut h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        // Descend into the credential stage first.
        descend_to_credential(&mut h, 0);
        h.app.on_paste("sk-123");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.dialog.as_ref().unwrap().input, "sk-123");
    }

    #[test]
    fn no_resolvable_key_opens_the_chat() {
        // Nothing gates the session anymore: a keyless config is a
        // chat whose footer says "no provider" in red; the wizard is
        // for the first run only.
        let h = harness(None, None, Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.initial_effects().is_empty());
    }

    /// A store that refuses everything: the locked-keychain shape the
    /// reachability probe exists to catch.
    struct LockedStore;

    impl rhema_config::secrets::SecretStore for LockedStore {
        fn reachable(&self) -> bool {
            false
        }

        fn get(&self, _id: &str) -> Result<Option<String>, rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }

        fn set(&self, _id: &str, _value: &str) -> Result<(), rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }

        fn delete(&self, _id: &str) -> Result<bool, rhema_config::secrets::SecretError> {
            Err(rhema_config::secrets::SecretError::NoStore)
        }
    }

    #[test]
    fn a_locked_keychain_is_named_by_the_missing_key_help() {
        let profile = rhema_config::Profile {
            provider: Some("google".into()),
            ..rhema_config::Profile::default()
        };
        let err = Backend::from_profile_or_help(&profile, Some(&LockedStore)).unwrap_err();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
        assert!(err.contains("locked or unreachable"), "{err}");
    }

    #[test]
    fn the_plain_missing_key_help_names_no_keychain() {
        let profile = rhema_config::Profile {
            provider: Some("google".into()),
            ..rhema_config::Profile::default()
        };
        let err = Backend::from_profile_or_help(&profile, None).unwrap_err();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
        assert!(!err.contains("keychain"), "{err}");
    }

    #[test]
    fn a_key_without_a_stored_model_opens_the_chat_quietly() {
        // No model stored is a red footer and /model one keystroke
        // away, not a gate: no fetch runs at startup and nothing but
        // the wizard stands between the user and the input box.
        let h = harness(Some("k"), None, Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_none());
        // The catalog still warms: ctrl-p answers from the cache the
        // moment the user asks for a model.
        assert_eq!(
            h.app.initial_effects(),
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        );
    }

    #[test]
    fn a_key_with_a_stored_model_opens_the_chat() {
        let h = harness(Some("k"), Some("m-a"), Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.app.initial_effects(),
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        );
    }

    /// A landed allowance rides with the backend that fetched it; a
    /// failed fetch clears the badge rather than leaving a stale
    /// figure that reads as truth.
    #[test]
    fn the_allowance_lands_and_a_failed_fetch_clears_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::UsageLoaded {
            provider: "google",
            result: Ok(Some(rhema_provider::Allowance {
                amount: "$4.12".to_owned(),
                kind: String::new(),
            })),
        });
        let (provider, allowance) = h.app.subscription.as_ref().expect("allowance landed");
        assert_eq!(*provider, "google");
        assert_eq!(allowance.as_ref().expect("present").amount, "$4.12");

        h.app.on_event(UiEvent::UsageLoaded {
            provider: "google",
            result: Err("network error".to_owned()),
        });
        assert!(h.app.subscription.is_none());
    }

    #[test]
    fn raced_nudges_chain_one_turn_per_finish_in_order() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (steer, rx) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
        for text in ["first", "second", "third"] {
            steer.blocking_send(text.into()).expect("queued");
        }
        drop(steer);
        let agent = Box::new(h.app.agent.take().unwrap());
        let effects = h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: rx,
        });
        // Exactly one turn leaves per finish; the rest wait their turn.
        // (The finish also refreshes usage; that rides along.)
        assert_eq!(
            effects,
            vec![
                Effect::StartTurn {
                    text: "first".into(),
                    images: Vec::new()
                },
                Effect::FetchUsage,
            ]
        );
        assert_eq!(
            Vec::from_iter(h.app.pending_prompts.iter()),
            ["second", "third"]
        );

        // The chained turn's finish sends the next one out.
        let agent = Box::new(h.app.agent.take().unwrap());
        let effects = h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert_eq!(
            effects,
            vec![
                Effect::StartTurn {
                    text: "second".into(),
                    images: Vec::new()
                },
                Effect::FetchUsage,
            ]
        );
        assert_eq!(Vec::from_iter(h.app.pending_prompts.iter()), ["third"]);

        // A cancel spends no queued words: they return to the input.
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnCancelled {
            agent,
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(h.app.pending_prompts.is_empty());
        assert_eq!(h.app.chat.input, "third");
    }

    /// The turn just moved the account's counters: finishing (or
    /// cancelling) one schedules the allowance refresh.
    #[test]
    fn a_finished_turn_refreshes_the_allowance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let agent = Box::new(h.app.agent.take().unwrap());
        let effects = h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert_eq!(effects, vec![Effect::FetchUsage]);
    }

    /// A reasoning-only response — a content-less assistant message the
    /// provider ended itself — cannot pass for an answer: the transcript
    /// names what happened. A reply that said something raises nothing.
    #[test]
    fn an_empty_turn_surfaces_its_stop_reason() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message {
                    role: Role::Assistant,
                    content: vec![],
                },
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(h.app.transcript.lines().contains(&Line::Notice(
            "the model returned no content (stopped: max tokens)".into()
        )));

        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        // The first turn's notice rightly persists; a real reply raised
        // no second one.
        assert_eq!(
            h.app
                .transcript
                .lines()
                .iter()
                .filter(|line| matches!(
                    line,
                    Line::Notice(text) if text.contains("no content")
                ))
                .count(),
            1
        );
    }

    /// A model switch lands a new backend: its allowance fetches at
    /// once, not at the first turn's end.
    #[test]
    fn a_model_switch_refreshes_the_allowance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let effects = h.app.choose_model("google", Some("google"), "m-a");
        assert_eq!(effects, vec![Effect::FetchUsage]);
    }

    #[test]
    fn login_success_persists_the_key_and_opens_the_picker() {
        let mut h = harness(None, None, Session::default());
        descend_to_credential(&mut h, 0);
        for c in "secret".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into()
                },
                instance: Some("google".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.models.len(), 2),
            _ => panic!("expected the picker after a successful login"),
        }
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.instance.as_deref(), Some("google"));
        assert_eq!(
            effective.instances["google"].api_key.as_deref(),
            Some("secret")
        );
        assert_eq!(effective.api_key("google", None).as_deref(), Some("secret"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::Google { ref api_key }) if api_key == "secret"
        ));
    }

    #[test]
    fn a_login_places_its_key_in_the_keychain_and_the_file_stays_clean() {
        let mut h = harness_with_secrets(None, None, Session::default());
        let secrets = Arc::clone(h.secrets.as_ref().unwrap());
        descend_to_credential(&mut h, 0);
        for c in "secret".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let _ = h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "secret".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        let effective = h.store.load().unwrap().effective(None);
        let instance = &effective.instances["google"];
        assert_eq!(instance.api_key, None, "the file holds no key");
        let id = instance.secret_ref.as_deref().expect("the keychain ref");
        assert_eq!(secrets.get(id).unwrap().as_deref(), Some("secret"));
        // The session's own resolution reads the chain: the login
        // serves turns with no literal anywhere.
        assert_eq!(
            h.app
                .effective()
                .api_key("google", h.app.secret_store())
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn removing_an_instance_deletes_its_keychain_entry() {
        let mut h = harness_with_secrets(Some("g"), Some("m"), Session::default());
        let secrets = Arc::clone(h.secrets.as_ref().unwrap());
        // The profile's one instance logs in through the chain: the
        // ref is live, the literal is gone.
        secrets.set("ref-1", "g").unwrap();
        h.app.config.set_for_profile(Some("personal"), |p| {
            let instance = p.instances.get_mut("google").unwrap();
            instance.secret_ref = Some("ref-1".into());
            instance.api_key = None;
        });
        // /login opens the providers page over the session's profile;
        // the instance row is selected, `d` forgets it.
        for c in "/login".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        let _ = h.app.on_key(key(KeyCode::Char('d')));
        let effective = h.store.load().unwrap().effective(None);
        assert!(!effective.instances.contains_key("google"), "forgotten");
        assert_eq!(secrets.get("ref-1").unwrap(), None, "the entry went too");
    }

    #[test]
    fn a_login_merges_its_catalog_instead_of_replacing_the_cache() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        // /login, then the openrouter row and a key.
        for c in "/login".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        descend_to_credential(&mut h, 2);
        for c in "o".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenRouter { .. },
                ..
            })]
        ));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenRouter {
                    api_key: "o".into(),
                },
                instance: Some("openrouter".into()),
            },
            result: loaded(
                "openrouter",
                vec![llama_model("openrouter/auto"), llama_model("z-ai/glm-5.3")],
            ),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.models.len(), 4, "google stays: {picker:?}");
                let providers: Vec<&str> = picker.models.iter().map(|m| m.provider).collect();
                assert_eq!(providers, ["google", "google", "openrouter", "openrouter"]);
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn a_late_bootstrap_keeps_the_login_models() {
        // The startup fetch races a login: it was spawned first, but a
        // slow catalog can land it after the login's answer. Its slices
        // replace themselves; the login's slice is not its business.
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        for c in "/login".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        descend_to_credential(&mut h, 2);
        for c in "o".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::FetchModels(FetchWhy::Login { .. })]
        ));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenRouter {
                    api_key: "o".into(),
                },
                instance: Some("openrouter".into()),
            },
            result: loaded(
                "openrouter",
                vec![llama_model("openrouter/auto"), llama_model("z-ai/glm-5.3")],
            ),
        });
        assert_eq!(h.app.models.len(), 4);

        // The late startup answer covers google only.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        let providers: Vec<&str> = h.app.models.iter().map(|m| m.provider).collect();
        assert_eq!(
            providers,
            ["google", "google", "openrouter", "openrouter"],
            "the login's slice survives the late bootstrap"
        );
    }

    #[test]
    fn llama_login_validates_by_server_url_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // The llama.cpp row is second on the add flow's provider list.
        let name = descend_to_credential(&mut h, 1);
        assert_eq!(name, "llama-cpp");
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(
                    login.pending.as_ref().map(|p| p.provider),
                    Some(crate::screens::LoginProvider::LlamaCpp)
                );
                assert_eq!(login.input_label(), "server URL: ");
            }
            _ => panic!("expected the login screen"),
        }
        for c in "http://box:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        // Enter captures the URL; the bearer key prompt follows, and a
        // blank one is accepted (an open server).
        h.app.on_key(key(KeyCode::Enter));
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(login.input_label(), "API key: ");
            }
            _ => panic!("expected the bearer-key prompt"),
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://box:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            },
            result: loaded(
                "llama-cpp",
                vec![llama_model("qwen3-8b"), llama_model("qwen3-32b")],
            ),
        });
        // Router mode: several models land in the picker with no
        // metadata, and the rows carry the llama.cpp prefix — not google's.
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(picker.models.iter().all(|m| m.provider == "llama-cpp"));
                assert_eq!(picker.models.len(), 2);
                let ids: Vec<&str> = picker.models.iter().map(|m| m.model.id.as_str()).collect();
                assert_eq!(ids, ["qwen3-32b", "qwen3-8b"]);
                assert!(picker.models[0].model.input_token_limit.is_none());
            }
            _ => panic!("expected the picker after a successful llama login"),
        }
        let config = h.store.load().unwrap();
        let effective = config.effective(None);
        assert_eq!(effective.provider.as_deref(), Some("llama-cpp"));
        assert_eq!(
            effective.instances["llama-cpp"].base_url.as_deref(),
            Some("http://box:8080")
        );
        assert_eq!(effective.llama_cpp_url(), "http://box:8080");
    }

    #[test]
    fn anthropic_login_validates_by_key_and_persists_the_provider() {
        let mut h = harness(None, None, Session::default());
        // Anthropic is the fourth provider row.
        let name = descend_to_credential(&mut h, 3);
        assert_eq!(name, "anthropic");
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(login.input_label(), "API key: ");
            }
            _ => panic!("expected the login screen"),
        }
        for c in "sk-ant-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Anthropic {
                    api_key: "sk-ant-01".into(),
                },
                instance: Some("anthropic".into()),
            })]
        );
        // The model-list fetch is the validation; success lands in the
        // picker prefixed `anthropic/`, and the key lands in the profile.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Anthropic {
                    api_key: "sk-ant-01".into(),
                },
                instance: Some("anthropic".into()),
            },
            result: loaded(
                "anthropic",
                vec![
                    llama_model("claude-sonnet-4-5"),
                    llama_model("claude-opus-4-1"),
                ],
            ),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(picker.models.iter().all(|m| m.provider == "anthropic"));
                assert_eq!(picker.models.len(), 2);
                // The Anthropic listing carries identity only: no
                // context, no pricing.
                assert!(picker.models[0].model.input_token_limit.is_none());
                assert!(picker.models[0].model.pricing.is_none());
            }
            _ => panic!("expected the picker after a successful anthropic login"),
        }
        let config = h.store.load().unwrap();
        let effective = config.effective(None);
        assert_eq!(effective.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            effective.instances["anthropic"].api_key.as_deref(),
            Some("sk-ant-01")
        );
        assert_eq!(
            effective.api_key("anthropic", None).as_deref(),
            Some("sk-ant-01")
        );
        assert!(matches!(
            h.app.backend,
            Some(Backend::Anthropic { ref api_key }) if api_key == "sk-ant-01"
        ));
    }

    #[test]
    fn openai_login_validates_by_key_with_the_default_base() {
        let mut h = harness(None, None, Session::default());
        // OpenAI is the fifth provider row.
        descend_to_credential(&mut h, 4);
        // The base URL prompt comes first: an empty one means OpenAI
        // itself.
        h.app.on_key(key(KeyCode::Enter));
        for c in "sk-proj-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-proj-01".into(),
                    base_url: None,
                },
                instance: Some("openai".into()),
            })]
        );
        // A failed validation stays on the login screen and persists
        // nothing — same contract as the other keyed providers.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-proj-01".into(),
                    base_url: None,
                },
                instance: Some("openai".into()),
            },
            result: failed("openai", "openai-compatible api error (HTTP 401): denied"),
        });
        match &h.app.screen {
            Screen::Settings(s) => assert!(matches!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error(_)
            )),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.app.backend.is_none());
        assert!(h.store.load().unwrap().effective(None).instances.is_empty());
    }

    #[test]
    fn zai_login_validates_by_key_with_the_default_route() {
        let mut h = harness(None, None, Session::default());
        // Z.ai is the seventh provider row.
        descend_to_credential(&mut h, 6);
        // The base URL prompt comes first: an empty one means the Coding
        // Plan route itself.
        h.app.on_key(key(KeyCode::Enter));
        for c in "zkey-01".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Zai {
                    api_key: "zkey-01".into(),
                    base_url: None,
                },
                instance: Some("zai".into()),
            })]
        );
        // A failed validation stays on the login screen and persists
        // nothing — same contract as the other keyed providers.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Zai {
                    api_key: "zkey-01".into(),
                    base_url: None,
                },
                instance: Some("zai".into()),
            },
            result: failed("zai", "anthropic api error (HTTP 401): denied"),
        });
        match &h.app.screen {
            Screen::Settings(s) => assert!(matches!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error(_)
            )),
            _ => panic!("a failed login must stay on the login screen"),
        }
        assert!(h.app.backend.is_none());
        assert!(h.store.load().unwrap().effective(None).instances.is_empty());
    }

    #[test]
    fn a_zai_profile_builds_its_own_backend_from_a_stored_route() {
        // A stored instance with a base URL override: the backend points
        // the adapter at that route instead of the Coding Plan's.
        let mut profile = keyed_profile("zai", "k-z");
        profile.instance = Some("zai".into());
        profile.upsert_instance(
            "proxy",
            rhema_config::ProviderInstance {
                provider: "zai".into(),
                api_key: Some("k-p".into()),
                secret_ref: None,
                base_url: Some("https://proxy.example/api/anthropic".into()),
            },
        );
        let backend =
            Backend::from_profile(&profile, None).expect("a keyed zai profile serves turns");
        assert_eq!(backend.config_name(), "zai");
        assert!(
            matches!(
                backend,
                Backend::Zai { ref api_key, base_url: None }
                    if api_key == "k-z"
            ),
            "the active zai instance serves: {backend:?}"
        );
        // Activating the proxy instance moves both the key and the route.
        profile.instance = Some("proxy".into());
        assert!(
            matches!(
                Backend::from_profile(&profile, None),
                Some(Backend::Zai { ref api_key, base_url: Some(ref url) })
                    if api_key == "k-p" && url == "https://proxy.example/api/anthropic"
            ),
            "the proxy instance's route serves"
        );
        // No key, no backend — the kind is not served by another's key.
        let mut cross = rhema_config::Profile {
            provider: Some("zai".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut cross, "anthropic", "k-a");
        assert_eq!(
            Backend::from_profile(&cross, None),
            None,
            "a zai profile is not served by an anthropic key"
        );
    }

    #[test]
    fn anthropic_and_openai_profiles_build_their_own_backends() {
        let keyed = |provider: &str, key: Option<&str>| {
            let mut profile = rhema_config::Profile {
                provider: Some(provider.to_owned()),
                ..rhema_config::Profile::default()
            };
            if let Some(key) = key {
                store_key(&mut profile, provider, key);
            }
            profile
        };
        let backend = Backend::from_profile(&keyed("anthropic", Some("k-a")), None)
            .expect("a keyed anthropic profile serves turns");
        assert_eq!(backend.config_name(), "anthropic");
        assert!(
            matches!(backend, Backend::Anthropic { ref api_key } if api_key == "k-a"),
            "the anthropic profile builds the anthropic backend: {backend:?}"
        );
        let backend = Backend::from_profile(&keyed("openai", Some("k-o")), None)
            .expect("a keyed openai profile serves turns");
        assert_eq!(backend.config_name(), "openai");
        assert!(
            matches!(backend, Backend::OpenAi { ref api_key, .. } if api_key == "k-o"),
            "the openai profile builds the openai backend: {backend:?}"
        );
        // No key, no backend — a keyless profile cannot serve turns.
        let mut cross = rhema_config::Profile {
            provider: Some("anthropic".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut cross, "openai", "k-o");
        assert_eq!(
            Backend::from_profile(&cross, None),
            None,
            "an anthropic profile is not served by an openai key"
        );
        assert_eq!(
            Backend::from_profile(
                &rhema_config::Profile {
                    provider: Some("openai".into()),
                    ..rhema_config::Profile::default()
                },
                None,
            ),
            None
        );
        // No provider named, and a spelling no provider answers to, both
        // leave the backend unresolved: the login flow runs, nothing
        // silently serves google.
        assert_eq!(
            Backend::from_profile(&rhema_config::Profile::default(), None),
            None,
            "a provider-less profile opens the login flow"
        );
        let mut nonsense = rhema_config::Profile {
            provider: Some("nonsense".into()),
            ..rhema_config::Profile::default()
        };
        store_key(&mut nonsense, "google", "k");
        assert_eq!(
            Backend::from_profile(&nonsense, None),
            None,
            "an unknown spelling never falls back to google"
        );
    }

    #[test]
    fn candidates_span_every_provider_the_profile_holds_keys_for() {
        let mut profile = keyed_profile("openrouter", "o");
        store_key(&mut profile, "google", "g");
        let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
            .iter()
            .map(|candidate| candidate.backend.config_name())
            .collect();
        assert_eq!(names, ["openrouter", "google"], "the active provider leads");

        // llama.cpp joins only as the active provider: a server address
        // is a login, not a catalog to browse.
        profile.provider = Some("llama-cpp".into());
        let names: Vec<&str> = Backend::candidates_from_profile(&profile, None)
            .iter()
            .map(|candidate| candidate.backend.config_name())
            .collect();
        assert_eq!(names, ["llama-cpp", "google", "openrouter"]);
    }

    #[test]
    fn candidates_name_every_stored_instance_and_put_the_active_one_first() {
        let profile = rhema_config::Profile {
            provider: Some("llama-cpp".to_owned()),
            instance: Some("gpu box".to_owned()),
            instances: BTreeMap::from([
                (
                    "gpu box".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        secret_ref: None,
                        base_url: Some("http://gpu:8080".into()),
                    },
                ),
                (
                    "home".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        secret_ref: None,
                        base_url: Some("http://home:8080".into()),
                    },
                ),
                (
                    "work sub".to_owned(),
                    rhema_config::ProviderInstance {
                        provider: "copilot".into(),
                        api_key: Some("gho-1".into()),
                        secret_ref: None,
                        base_url: None,
                    },
                ),
            ]),
            ..rhema_config::Profile::default()
        };
        let candidates = Backend::candidates_from_profile(&profile, None);
        let rows: Vec<(Option<&str>, &str)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.instance.as_deref(),
                    candidate.backend.config_name(),
                )
            })
            .collect();
        // The active instance leads, the rest follow in name order —
        // every llama.cpp server joins, a stored URL being an explicit
        // login now.
        assert_eq!(
            rows,
            [
                (Some("gpu box"), "llama-cpp"),
                (Some("home"), "llama-cpp"),
                (Some("work sub"), "copilot"),
            ]
        );
        assert!(
            matches!(
                candidates[0].backend,
                Backend::LlamaCpp { ref url, .. } if url == "http://gpu:8080"
            ),
            "the active instance's own URL serves: {:?}",
            candidates[0].backend
        );
    }

    #[test]
    fn refresh_merges_every_providers_catalog_and_names_the_failures() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: [
                loaded("google", models()),
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
            ]
            .concat(),
        });
        h.app.on_key(ctrl('p'));
        // A refresh where google's catalog fails: the openrouter one keeps
        // the picker alive, and the footer names the gap.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: [
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
                failed("google", "gemini api error (HTTP 403): denied"),
            ]
            .concat(),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert_eq!(picker.status, PickerStatus::Ready);
                assert_eq!(picker.failed, ["google"]);
                assert_eq!(picker.models.len(), 1);
                assert_eq!(picker.models[0].provider, "openrouter");
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn a_total_catalog_failure_is_the_pickers_error() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.on_key(ctrl('p'));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: failed("google", "gemini api error (HTTP 403): denied"),
        });
        match &h.app.screen {
            Screen::Picker(picker) => {
                assert!(matches!(picker.status, PickerStatus::Error(_)));
            }
            _ => panic!("expected the picker"),
        }
    }

    #[test]
    fn slash_model_with_a_needle_switches_directly() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        for c in "/model m-b".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchUsage],
            "a named model rebuilds inline"
        );
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-b")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("model switched to google/m-b".into()))
        );
    }

    #[test]
    fn slash_model_with_a_unique_prefix_resolves_and_an_unknown_needle_says_so() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        for c in "/model m-".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(h.app.screen, Screen::Chat),
            "the picker never opens"
        );
        assert!(
            h.app
                .transcript
                .lines()
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("2 models match"))),
            "ambiguous prefix says so"
        );
        for c in "/model nope".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            h.app.transcript.lines().iter().any(
                |line| matches!(line, Line::Notice(text) if text.contains("no model matches"))
            )
        );
    }

    #[test]
    fn a_resumed_session_continues_its_cost() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "earlier work"));
        session.cost_usd = 2.5;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        crate::sessions::write_session(&path, &session).unwrap();

        h.app.resume_session(&path);
        assert_eq!(h.app.session_cost, 2.5, "the footer continues the story");
        // A turn on top of it accumulates further, and the exit save
        // carries the total back into the file.
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.model = Some("m-a".into())
        });
        let (exported, origin) = h.app.export_session().unwrap();
        assert_eq!(origin.as_deref(), Some(path.as_path()));
        assert_eq!(exported.cost_usd, 2.5);
    }

    #[test]
    fn a_resumed_session_shows_its_saved_usage() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "earlier work"));
        let saved_stats = rhema_transcript::TranscriptStats {
            requests: 7,
            input_tokens: 90_000,
            cached_input_tokens: 70_000,
            output_tokens: 1_000,
            usage: rhema_transcript::UsageBreakdown {
                system_prompt_tokens: 20_000,
                ..rhema_transcript::UsageBreakdown::default()
            },
            ..rhema_transcript::TranscriptStats::default()
        };
        let last_turn = Usage {
            input_tokens: 3_000,
            output_tokens: 300,
            cached_input_tokens: 2_000,
        };
        crate::sessions::UsageStats {
            transcript: Some(saved_stats),
            last_turn: Some(last_turn),
            transcript_sections: None,
        }
        .write_into(&mut session);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        crate::sessions::write_session(&path, &session).unwrap();

        h.app.resume_session(&path);
        // The gauge picks up the last turn, the mirror the counters.
        assert_eq!(h.app.usage, Some(last_turn));
        assert_eq!(
            h.app.agent.as_ref().unwrap().transcript_stats(),
            Some(saved_stats)
        );
        // And /settings's Usage page reads the whole thing.
        h.app.open_settings_at(SettingsSection::Usage);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("settings should be open");
        };
        assert_eq!(settings.last_turn, Some(last_turn));
        assert_eq!(settings.transcript, Some(saved_stats));
        assert_eq!(settings.session_cost, 0.0);
    }

    #[test]
    fn the_saved_usage_survives_a_save_and_resume_round_trip() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "earlier work"));
        let saved_stats = rhema_transcript::TranscriptStats {
            requests: 7,
            input_tokens: 90_000,
            output_tokens: 1_000,
            ..rhema_transcript::TranscriptStats::default()
        };
        crate::sessions::UsageStats {
            transcript: Some(saved_stats),
            last_turn: None,
            transcript_sections: None,
        }
        .write_into(&mut session);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        crate::sessions::write_session(&path, &session).unwrap();

        // Resume, then save: the exit save re-stamps what the run
        // measured — the carried counters, nothing lost to the fresh
        // mirror starting at zero.
        h.app.resume_session(&path);
        let (exported, _) = h.app.export_session().unwrap();
        assert_eq!(
            crate::sessions::UsageStats::from_session(&exported)
                .unwrap()
                .transcript,
            Some(saved_stats)
        );

        // And what was saved, a second resume shows.
        crate::sessions::write_session(&path, &exported).unwrap();
        let mut second = harness(Some("k"), Some("m-a"), Session::default());
        second.app.resume_session(&path);
        assert_eq!(
            second.app.agent.unwrap().transcript_stats(),
            Some(saved_stats)
        );
    }

    #[test]
    fn a_turn_on_a_resumed_session_extends_the_saved_counters() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut session = Session::default();
        session
            .history
            .push(Message::text(Role::User, "earlier work"));
        let baseline = rhema_transcript::TranscriptStats {
            requests: 7,
            input_tokens: 90_000,
            output_tokens: 1_000,
            ..rhema_transcript::TranscriptStats::default()
        };
        crate::sessions::UsageStats {
            transcript: Some(baseline),
            last_turn: None,
            transcript_sections: None,
        }
        .write_into(&mut session);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        crate::sessions::write_session(&path, &session).unwrap();
        h.app.resume_session(&path);

        // A turn ran elsewhere; its own mirror counted one request.
        // Folding it at capture keeps the run's totals.
        let mut agent = h.app.agent.take().unwrap();
        agent.carry_transcript_stats(&rhema_transcript::TranscriptStats {
            requests: 2,
            input_tokens: 5_000,
            output_tokens: 400,
            ..rhema_transcript::TranscriptStats::default()
        });
        h.app.on_event(UiEvent::TurnFinished {
            agent: Box::new(agent),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        let stats = h.app.transcript_stats.unwrap();
        assert_eq!(stats.requests, 9, "the run's requests, not the turn's");
        assert_eq!(stats.input_tokens, 95_000);
        assert_eq!(stats.output_tokens, 1_400);
    }

    #[test]
    fn slash_clear_drops_the_usage_accounting() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let baseline = rhema_transcript::TranscriptStats {
            requests: 7,
            input_tokens: 90_000,
            ..rhema_transcript::TranscriptStats::default()
        };
        h.app
            .agent
            .as_mut()
            .unwrap()
            .carry_transcript_stats(&baseline);
        h.app.usage = Some(Usage::default());
        crate::commands::dispatch(&mut h.app, "/clear");
        assert_eq!(h.app.usage, None);
        assert_eq!(h.app.transcript_stats, None);
        // The mirror starts over too: the page cannot quote the
        // cleared conversation.
        assert_eq!(
            h.app.agent.unwrap().transcript_stats(),
            Some(rhema_transcript::TranscriptStats::default())
        );
    }

    #[test]
    fn a_profile_switch_keeps_the_usage_counters() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            store_key(settings, "openrouter", "o");
        });
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "o"));
        let baseline = rhema_transcript::TranscriptStats {
            requests: 7,
            input_tokens: 90_000,
            ..rhema_transcript::TranscriptStats::default()
        };
        h.app
            .agent
            .as_mut()
            .unwrap()
            .carry_transcript_stats(&baseline);
        for c in "/profile work".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        let stats = h.app.agent.unwrap().transcript_stats().unwrap();
        assert_eq!(stats.requests, 7, "the counters moved with the chat");
    }

    #[test]
    fn slash_profile_switches_to_a_named_profile() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            store_key(settings, "openrouter", "o");
        });
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "o"));
        for c in "/profile work".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(
            h.app.backend,
            Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
        ));
    }

    #[test]
    fn choosing_from_another_provider_switches_the_backend() {
        let mut h = harness(Some("g"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            store_key(settings, "openrouter", "o");
        });
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: [
                loaded("google", models()),
                loaded("openrouter", vec![llama_model("openrouter/auto")]),
            ]
            .concat(),
        });
        // Filter by provider, pick the openrouter model.
        h.app.on_key(ctrl('p'));
        for c in "openrouter".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::FetchUsage], "a pick rebuilds inline");
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.backend,
            Some(Backend::OpenRouter { ref api_key }) if api_key == "o"
        ));
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.provider.as_deref(), Some("openrouter"));
        assert_eq!(effective.model.as_deref(), Some("openrouter/auto"));
        assert!(h.app.transcript.lines().contains(&Line::Notice(
            "model switched to openrouter/openrouter/auto".into()
        )));
    }

    #[test]
    fn turn_costs_accumulate_from_the_model_pricing() {
        // No stored model: the chat opens bare and the test names the
        // priced model through /model — the move the red footer
        // invites — before the turn runs.
        let mut h = harness(
            Some("k"),
            None,
            Session {
                history: vec![Message::text(Role::User, "earlier work")],
                driver_state: serde_json::Value::Null,
                cost_usd: 0.0,
                usage_stats: serde_json::Value::Null,
            },
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            // $0.75/M in, $3.75/M out, cached input at a quarter — the
            // shape the google adapter bakes for a flash model.
            result: loaded(
                "google",
                vec![priced_model(
                    "gemini-3.8-flash",
                    rhema_provider::Pricing {
                        input_per_mtok: 0.75,
                        output_per_mtok: 3.75,
                        cached_input_per_mtok: Some(0.1875),
                    },
                )],
            ),
        });
        crate::commands::dispatch(&mut h.app, "/model gemini-3.8-flash");
        assert!(h.app.agent.is_some(), "the pick built the agent");
        h.app.on_key(key(KeyCode::Char('a')));
        let effects = h.app.on_key(key(KeyCode::Enter));
        match effects.as_slice() {
            [Effect::StartTurn { images, .. }] => {
                assert!(images.is_empty());
            }
            other => panic!("expected a turn start, got {other:?}"),
        }
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    cached_input_tokens: 400_000,
                },
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        // 600k uncached in at $0.75/M + 400k cached in at a quarter of
        // that + 1M out at $3.75/M.
        assert!((h.app.session_cost - 4.275).abs() < 1e-9);
        // The exit save carries the total, so a resume continues it.
        let (exported, _) = h.app.export_session().unwrap();
        assert!((exported.cost_usd - 4.275).abs() < 1e-9);
    }

    #[test]
    fn a_single_model_server_auto_selects_and_skips_the_picker() {
        let mut h = harness(None, None, Session::default());
        // The llama.cpp row, the suggested name, the URL, and a blank
        // bearer key.
        descend_to_credential(&mut h, 1);
        for c in "http://localhost:8080".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::LlamaCpp {
                    url: "http://localhost:8080".into(),
                    api_key: None,
                },
                instance: Some("llama-cpp".into()),
            },
            result: loaded("llama-cpp", vec![llama_model("qwen3-8b")]),
        });
        // Exactly one model: no picker, straight to chat on that model.
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("qwen3-8b")
        );
    }

    #[test]
    fn enter_on_a_stored_instance_activates_it() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("llama-cpp".into());
            settings.instance = Some("home".into());
            settings.model = Some("qwen3-8b".into());
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
            settings.upsert_instance(
                "gpu box",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://gpu:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "gpu box" sorts before "home": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchUsage],
            "a stored model goes straight to chat"
        );
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(
            matches!(&h.app.backend, Some(Backend::LlamaCpp { url, .. }) if url == "http://gpu:8080"),
            "the activated instance's own URL serves: {:?}",
            h.app.backend
        );
        let effective = h.store.load().unwrap().effective(None);
        assert_eq!(effective.instance.as_deref(), Some("gpu box"));
        assert_eq!(
            effective.model.as_deref(),
            Some("qwen3-8b"),
            "the same provider keeps its stored model"
        );
    }

    #[test]
    fn activating_across_providers_drops_the_foreign_model() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("openai".into());
            settings.instance = Some("openai".into());
            settings.model = Some("gpt-5".into());
            settings.upsert_instance(
                "openai",
                rhema_config::ProviderInstance {
                    provider: "openai".into(),
                    api_key: Some("sk".into()),
                    secret_ref: None,
                    base_url: None,
                },
            );
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        // "home" sorts before "openai": the first row is it.
        let effects = h.app.on_key(key(KeyCode::Enter));
        // gpt-5 means nothing to a llama.cpp server: the picker opens.
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        );
        assert!(matches!(h.app.screen, Screen::Picker(_)));
        assert_eq!(
            h.store.load().unwrap().effective(None).model,
            None,
            "the foreign model id did not survive the switch"
        );
    }

    #[test]
    fn d_forgets_the_highlighted_instance() {
        let mut h = harness(None, None, Session::default());
        h.app.config.set_for_profile(Some("personal"), |settings| {
            settings.provider = Some("llama-cpp".into());
            settings.instance = Some("home".into());
            settings.upsert_instance(
                "home",
                rhema_config::ProviderInstance {
                    provider: "llama-cpp".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: Some("http://home:8080".into()),
                },
            );
        });
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        let effective = h.store.load().unwrap().effective(None);
        assert!(effective.instances.is_empty());
        assert_eq!(effective.instance, None);
        // The provider stays llama-cpp, whose keyless server always
        // constructs — on the default URL now, the way a flat profile
        // always could.
        assert!(matches!(
            h.app.backend,
            Some(Backend::LlamaCpp { ref url, .. }) if url == "http://localhost:8080"
        ));
    }

    #[test]
    fn a_second_openai_compatible_instance_lands_under_its_own_name() {
        let mut h = harness(None, None, Session::default());
        // The openai row: a custom base URL, then the key.
        descend_to_credential(&mut h, 4);
        for c in "http://proxy:1234/v1".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        for c in "sk-p".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-p".into(),
                    base_url: Some("http://proxy:1234/v1".into()),
                },
                instance: Some("openai".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenAi {
                    api_key: "sk-p".into(),
                    base_url: Some("http://proxy:1234/v1".into()),
                },
                instance: Some("openai".into()),
            },
            result: loaded("openai", vec![llama_model("gpt-5")]),
        });
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(
            matches!(&h.app.backend, Some(Backend::OpenAi { base_url: Some(url), .. }) if url == "http://proxy:1234/v1"),
            "the custom base URL serves: {:?}",
            h.app.backend
        );

        // A second login of the same provider suggests a fresh name.
        crate::commands::dispatch(&mut h.app, "/login");
        let name = descend_to_credential(&mut h, 4);
        assert_eq!(name, "openai-2", "the suggestion skips the taken name");
    }

    #[test]
    fn login_failure_renders_inline_and_persists_nothing() {
        let mut h = harness(None, None, Session::default());
        descend_to_credential(&mut h, 0);
        for c in "bad-key".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "bad-key".into(),
                },
                instance: Some("google".into()),
            },
            result: failed("google", "gemini api error (HTTP 403): denied"),
        });
        match &h.app.screen {
            Screen::Settings(s) => assert_eq!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error("gemini api error (HTTP 403): denied".into())
            ),
            _ => panic!("a failed login must stay on the login screen"),
        }
        let stored = h.store.load().unwrap();
        assert!(
            stored
                .profiles
                .values()
                .all(|profile| profile.instances.is_empty())
        );
    }

    #[test]
    fn the_copilot_login_runs_a_flow_and_persists_its_credential() {
        let mut h = harness(None, None, Session::default());
        // Walk down to the GitHub Copilot row (the last), name the
        // instance, and enter: the effect starts the flow, not a
        // credential input.
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_key(key(KeyCode::Enter));
        for _ in 0..crate::screens::PROVIDERS.len() - 1 {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(login.stage, crate::screens::DialogStage::Flow);
                assert_eq!(
                    login.pending.as_ref().map(|p| p.provider),
                    Some(LoginProvider::Copilot)
                );
            }
            _ => panic!("expected the login screen in its flow stage"),
        }

        // The flow's instruction and progress render on the screen.
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Instruct(
                rhema_provider::LoginInstruction {
                    text: "open the page".into(),
                    url: Some("https://github.com/login/device".into()),
                    code: Some("ABCD-1234".into()),
                },
            )));
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Waiting(
                "waiting".into(),
            )));
        match &h.app.screen {
            Screen::Settings(s) => {
                let login = s.dialog.as_ref().unwrap();
                assert_eq!(login.flow_waiting.as_deref(), Some("waiting"));
                assert!(login.flow_instruction.is_some());
            }
            _ => panic!("expected the login screen mid-flow"),
        }

        // Completion validates the credential through the fetch path.
        let effects = h.app.on_event(UiEvent::LoginUpdate(
            rhema_provider::LoginUpdate::Completed("gho_token".into()),
        ));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Copilot {
                    api_key: "gho_token".into(),
                },
                instance: Some("copilot".into()),
            })]
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Copilot {
                    api_key: "gho_token".into(),
                },
                instance: Some("copilot".into()),
            },
            result: loaded("copilot", models()),
        });
        match &h.app.screen {
            Screen::Picker(picker) => assert_eq!(picker.models.len(), 2),
            _ => panic!("expected the picker after a successful flow login"),
        }
        assert_eq!(
            h.store
                .load()
                .unwrap()
                .effective(None)
                .api_key("copilot", None)
                .as_deref(),
            Some("gho_token")
        );
        assert!(matches!(
            h.app.backend,
            Some(Backend::Copilot { ref api_key }) if api_key == "gho_token"
        ));
    }

    #[test]
    fn a_failed_flow_renders_its_error_and_persists_nothing() {
        let mut h = harness(None, None, Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        for _ in 0..crate::screens::PROVIDERS.len() - 1 {
            h.app.on_key(key(KeyCode::Down));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app
            .on_event(UiEvent::LoginUpdate(rhema_provider::LoginUpdate::Failed(
                "authorization was denied on GitHub".into(),
            )));
        match &h.app.screen {
            Screen::Settings(s) => assert_eq!(
                s.dialog.as_ref().unwrap().status,
                crate::screens::DialogStatus::Error("authorization was denied on GitHub".into())
            ),
            _ => panic!("expected the login screen"),
        }
        let stored = h.store.load().unwrap();
        assert!(
            stored
                .profiles
                .values()
                .all(|profile| profile.instances.is_empty())
        );
        // Esc closes the dialog back onto the Providers page for a retry.
        h.app.on_key(key(KeyCode::Esc));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_none());
    }

    #[test]
    fn choosing_a_model_persists_it_and_builds_the_agent() {
        let mut h = harness(Some("k"), None, Session::default());
        assert!(matches!(h.app.screen, Screen::Chat));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.on_key(ctrl('p'));
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::FetchUsage]);
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-a")
        );
    }

    #[test]
    fn ctrl_p_rebuild_preserves_the_session() {
        let session = Session {
            history: vec![Message::text(Role::User, "earlier work")],
            driver_state: serde_json::Value::Null,
            cost_usd: 0.0,
            usage_stats: serde_json::Value::Null,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        // Warm cache: no refetch effect.
        assert!(h.app.on_key(ctrl('p')).is_empty());
        assert!(matches!(&h.app.screen, Screen::Picker(_)));
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));

        assert!(matches!(h.app.screen, Screen::Chat));
        let agent = h.app.agent.as_ref().expect("rebuilt agent");
        assert_eq!(
            agent.session().history,
            vec![Message::text(Role::User, "earlier work")]
        );
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-b")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("model switched to google/m-b".into()))
        );
    }

    #[test]
    fn esc_when_idle_asks_and_y_quits() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.confirm_quit);
        assert!(!h.app.should_quit);
        h.app.on_key(key(KeyCode::Char('n')));
        assert!(!h.app.confirm_quit);
        h.app.on_key(key(KeyCode::Esc));
        h.app.on_key(key(KeyCode::Char('y')));
        assert!(h.app.should_quit);
    }

    #[test]
    fn the_context_footprint_estimates_a_loaded_history() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // No reported usage and no history: zero, as before.
        assert_eq!(h.app.context_used(), 0);

        // A resumed history has already spent context: eight characters
        // estimate to two tokens. The agent owns the live history.
        h.app
            .agent
            .as_mut()
            .unwrap()
            .session_mut()
            .history
            .push(Message::text(Role::User, "12345678"));
        assert_eq!(h.app.context_used(), 2);

        // An image prices as vision tokens, not as its base64 bytes:
        // a megabyte of screenshot data is one image on the gauge.
        h.app
            .agent
            .as_mut()
            .unwrap()
            .session_mut()
            .history
            .push(Message::text_with_images(
                Role::User,
                "see this",
                vec![rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "A".repeat(1_000_000),
                )],
            ));
        assert_eq!(h.app.context_used(), 2 + 2 + 1_000);

        // A reported turn wins over the estimate: prompt plus reply,
        // the shape of the next request.
        h.app.usage = Some(Usage {
            input_tokens: 500,
            output_tokens: 3,
            cached_input_tokens: 0,
        });
        assert_eq!(h.app.context_used(), 503);
    }

    #[test]
    fn the_live_usage_tally_moves_the_gauge_mid_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app
            .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);

        // The provider's running tally lands as an engine event: its
        // prompt count already covers the whole conversation, so the
        // gauge reads prompt plus the completion streamed so far.
        h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
            input_tokens: 400,
            output_tokens: 7,
            cached_input_tokens: 0,
        })));
        assert_eq!(h.app.context_used(), 407);

        // A later frame replaces the tally — never adds to it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
            input_tokens: 400,
            output_tokens: 19,
            cached_input_tokens: 0,
        })));
        assert_eq!(h.app.context_used(), 419);

        // A zero tally is a placeholder, not a measurement: the gauge
        // keeps the last real figure instead of collapsing to 0.
        h.app.on_event(UiEvent::Engine(OwnedEvent::Usage(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        })));
        assert_eq!(h.app.context_used(), 419);
    }

    #[test]
    fn the_status_tracks_thinking_versus_running() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app
            .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);

        // The step begins: thinking.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
        assert!(!h.app.turn.processing);

        // A tool call arrives: running it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.processing);

        // The result lands: still running, until the next step or the end.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c1".into(),
            content: "on main".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(h.app.turn.processing);

        // The turn ends: idle, no phase.
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.turn.processing);
    }

    #[test]
    fn paste_lands_in_the_input_with_newlines() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_paste("a\r\nb");
        assert_eq!(h.app.chat.input, "a\nb");
    }

    #[test]
    fn the_wheel_scrolls_the_same_step_as_the_page_keys() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_mouse_scroll(true);
        assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
        h.app.on_key(key(KeyCode::PageUp));
        assert_eq!(
            h.app.chat.scroll,
            2 * crate::screens::ChatState::SCROLL_STEP
        );
        h.app.on_mouse_scroll(false);
        assert_eq!(h.app.chat.scroll, crate::screens::ChatState::SCROLL_STEP);
    }

    #[test]
    fn esc_when_running_cancels_the_turn_and_flags_the_channel() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        h.app.turn.cancel = Some(cancel);
        h.app.turn.running = true;
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.turn.decline_all, "upcoming approvals decline");
        assert!(!h.app.should_quit, "esc is not quit mid-turn");
        assert!(*cancelled.borrow(), "the turn task sees the flag");

        // The task's event closes the turn out.
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.turn.running);
        assert!(h.app.turn.cancel.is_none());
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cancelled")
        ));
    }

    #[test]
    fn esc_with_a_pending_modal_cancels_and_releases_the_blocked_call() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (hook, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || hook.approve(&test_call()));
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(
            cancel,
            tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0,
            Some(answers),
        );
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        h.app.turn.cancel = Some(cancel);
        h.app.turn.running = true;

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.turn.pending_approval.is_some());
        h.app.on_key(key(KeyCode::Esc));
        assert!(!handle.join().unwrap(), "the parked call was declined");
        assert!(
            *cancelled.borrow(),
            "the turn was cancelled, not just declined"
        );
        assert!(h.app.turn.pending_approval.is_none());
    }

    #[test]
    fn the_cancelled_event_hands_the_agent_back_with_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k".to_owned())),
            CodingAgentConfig::new("m-a", PathBuf::from(".")),
        )
        .unwrap();
        h.app.on_event(UiEvent::TurnCancelled {
            agent: Box::new(agent),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.turn.running);
        assert!(h.app.agent.is_some());
        assert!(h.app.turn.cancel.is_none());
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cancelled")
        ));
    }

    #[test]
    fn the_modal_answer_flows_back_to_the_blocked_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (hook, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || hook.approve(&test_call()));
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(
            cancel,
            tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0,
            Some(answers),
        );

        let event = rx.blocking_recv().expect("the parked call requested");
        h.app.on_event(event);
        assert!(h.app.turn.pending_approval.is_some());
        h.app.on_key(key(KeyCode::Char('y')));
        assert!(handle.join().unwrap(), "the y key approves the parked call");
    }

    #[test]
    fn turn_finished_records_usage_and_errors() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let make_agent = || {
            Box::new(
                CodingAgent::new(
                    Arc::new(GoogleProvider::new("k")),
                    CodingAgentConfig::new("m-a", "."),
                )
                .unwrap(),
            )
        };
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            cached_input_tokens: 4,
        };
        h.app.turn.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage,
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.turn.running);
        assert!(h.app.error.is_none());
        assert_eq!(h.app.usage, Some(usage));

        h.app.turn.running = true;
        h.app.on_event(UiEvent::TurnFinished {
            agent: make_agent(),
            result: Err("stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit".into()),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.turn.running);
        assert_eq!(
            h.app.error.as_deref(),
            Some(
                "stopped after 4 steps without finishing; send a follow-up to keep going, or raise the step limit"
            )
        );
    }

    #[test]
    fn chat_submit_guards() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        // An empty input never submits.
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(!h.app.turn.running);
        // While a turn runs, typing works and Enter steers: the input
        // clears, the user line shows, and the effect notes the
        // interruption on the status strip.
        h.app.turn.running = true;
        for c in "hi".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(h.app.chat.input, "hi");
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::SteerTurn("hi".into())]
        );
        assert!(h.app.turn.running);
        assert!(h.app.chat.input.is_empty());
        // Idle again: Enter submits a fresh turn, shows the user line,
        // and blocks re-entry.
        h.app.turn.running = false;
        for c in "hi".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::StartTurn {
                text: "hi".into(),
                images: Vec::new()
            }]
        );
        assert!(h.app.turn.running);
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User {
                text: "hi".into(),
                images: Vec::new()
            })
        );
        // An empty input still never submits or steers.
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn a_submit_travels_embedded_and_lands_in_the_transcript() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let long = "l1\nl2\nl3\nl4\nl5";
        h.app.chat.paste(long);
        let embedded = rhema_core::paste::embed(&ChatState::MARK.to_string(), &[long.to_owned()]);
        // The turn starts on the session form, and the transcript line
        // carries it — the chip survives the send.
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::StartTurn {
                text: embedded.clone(),
                images: Vec::new()
            }]
        );
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User {
                text: embedded,
                images: Vec::new()
            })
        );
    }

    /// A command with an image attached is a prompt with an image, not
    /// a command: images route to a turn unconditionally.
    #[test]
    fn a_submission_carrying_images_is_never_a_command() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
        h.app.chat.input = format!("/help {}", ChatState::MARK);
        h.app.chat.chips = vec![crate::screens::Chip::Image(image.clone())];
        h.app.chat.cursor = h.app.chat.input.chars().count();
        match h.app.on_key(key(KeyCode::Enter)).as_slice() {
            [Effect::StartTurn { text, images }] => {
                assert_eq!(text, "/help ");
                assert_eq!(images.first(), Some(&image));
            }
            other => panic!("an image-bearing submit is a turn, not a command: {other:?}"),
        }
        assert_eq!(
            h.app.transcript.lines().last(),
            Some(&Line::User {
                text: "/help ".into(),
                images: vec![image],
            })
        );
    }

    /// Steering stays text: an image mid-turn keeps everything in the
    /// box for the next submit rather than stripping the image from a
    /// nudge.
    #[test]
    fn a_full_steer_queue_refuses_the_nudge_and_keeps_the_line() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let (steer, _rx) = tokio::sync::mpsc::channel(STEER_QUEUE_CAP);
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app.begin_turn(cancel, steer, None);
        for i in 0..STEER_QUEUE_CAP {
            h.app.chat.input = format!("nudge {i}");
            let effects = h.app.steer_turn(h.app.chat.input.clone());
            assert_eq!(effects.len(), 1, "queued nudge {i} still steers");
        }
        // The queue is full: the next nudge returns to the input.
        h.app.chat.input = "one too many".into();
        let effects = h.app.steer_turn("one too many".into());
        assert!(effects.is_empty(), "a full queue refuses: {effects:?}");
        assert_eq!(h.app.chat.input, "one too many");
        assert_eq!(
            h.app.chat.cursor,
            "one too many".chars().count(),
            "the cursor rides the restored line"
        );
    }

    #[test]
    fn an_image_cannot_steer_a_running_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let image = rhema_core::Image::new(rhema_core::ImageMediaType::Png, "aGVsbG8=");
        h.app.chat.insert_str("look");
        h.app.chat.paste_image(image);
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(!h.app.chat.input.is_empty(), "the text stays");
        assert_eq!(h.app.chat.chips.len(), 1, "the image stays");
        assert!(h.app.turn.running);
    }

    /// A running command's output fills the preview buffer from its
    /// first chunk; the next call or the result takes it back.
    #[test]
    fn a_running_commands_output_previews_and_goes_with_its_result() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
            id: "c1".into(),
            chunk: "one\n".into(),
        }));
        assert_eq!(h.app.turn.tool_output, "one\n");
        assert!(
            h.app.turn.tool_output_at.is_some(),
            "the first chunk clocks in"
        );

        // The next command takes the last one's preview with it.
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c2".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.tool_output.is_empty());
        assert!(h.app.turn.tool_output_at.is_none());

        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolOutputDelta {
            id: "c2".into(),
            chunk: "two\n".into(),
        }));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolResult {
            id: "c2".into(),
            content: "ok".into(),
            is_error: false,
            code: Some(0),
        }));
        assert!(h.app.turn.tool_output.is_empty());
        assert!(h.app.turn.tool_output_at.is_none());
    }

    #[test]
    fn engine_events_stream_into_the_transcript() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta("partial".into())));
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta(" answer".into())));
        assert_eq!(
            h.app.transcript.lines(),
            &[Line::Assistant("partial answer".into())]
        );
    }

    #[test]
    fn reasoning_previews_and_vanishes_when_output_starts() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("hmm".into())));
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta(" deeper".into())));
        assert_eq!(h.app.turn.thinking, "hmm deeper");

        // Real output clears the buffer: the block vanishes.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::TextDelta("the answer".into())));
        assert!(h.app.turn.thinking.is_empty());

        // So does a tool call mid-turn.
        h.app
            .on_event(UiEvent::Engine(OwnedEvent::ThinkingDelta("again".into())));
        h.app.on_event(UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        assert!(h.app.turn.thinking.is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit_from_any_screen() {
        let mut h = harness(None, None, Session::default());
        h.app.on_key(ctrl('c'));
        assert!(h.app.should_quit);

        let mut h = harness(None, None, Session::default());
        h.app.on_key(ctrl('d'));
        assert!(h.app.should_quit);
    }

    #[test]
    fn login_success_writes_into_the_selected_tab_profile() {
        let mut h = harness(None, None, Session::default());
        // Give the config a second profile so the login has a work tab.
        h.app
            .config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());
        // The session moves to the work profile, whose Providers page
        // the settings then edit, and an openrouter instance lands there.
        crate::commands::dispatch(&mut h.app, "/profile work");
        crate::commands::dispatch(&mut h.app, "/login");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.profile, "work");
        descend_to_credential(&mut h, 2);
        for c in "work-key".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::OpenRouter {
                    api_key: "work-key".into(),
                },
                instance: Some("openrouter".into()),
            },
            result: loaded("google", models()),
        });
        // The session now runs on work, and the key landed in its section.
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        let stored = h.store.load().unwrap();
        let work = stored.effective(Some("work"));
        assert_eq!(work.instance.as_deref(), Some("openrouter"));
        assert_eq!(
            work.instances["openrouter"].api_key.as_deref(),
            Some("work-key")
        );
        assert_eq!(
            work.api_key("openrouter", None).as_deref(),
            Some("work-key")
        );
    }

    #[test]
    fn slash_profile_with_a_name_switches_the_backend() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.profiles.insert(
            "work".to_owned(),
            rhema_config::Profile {
                model: Some("work-model".into()),
                ..keyed_profile("openrouter", "work-key")
            },
        );
        crate::commands::dispatch(&mut h.app, "/profile work");
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(h.app.screen, Screen::Chat), "stays in the chat");
        match &h.app.backend {
            Some(Backend::OpenRouter { api_key }) => assert_eq!(api_key, "work-key"),
            other => panic!("expected the openrouter backend, got {other:?}"),
        }
        // The profiles' sections are untouched, but the explicit choice
        // is recorded for this workspace right away.
        let stored = h.store.load().unwrap();
        assert!(stored.profiles.contains_key("personal"));
        assert_eq!(
            stored.last_profile_in(&h.app.options.workspace).as_deref(),
            Some("work")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("switched to profile work".into()))
        );
    }

    #[test]
    fn startup_without_a_flag_starts_on_a_ready_profile() {
        let work_ready = rhema_config::Profile {
            model: Some("work-model".into()),
            ..keyed_profile("openrouter", "work-key")
        };
        // personal is empty, work is logged in: the session starts on
        // work instead of the login screen.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "work"
        );
        // personal can serve turns: the default stands.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed_profile("google", "k")),
                ("work".to_owned(), work_ready.clone()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "personal"
        );
        // Nothing can: the default stands and the login screen opens on
        // it.
        assert_eq!(
            startup_profile(&Config::default(), Path::new("/nowhere"), None),
            "personal"
        );
        // A key without a provider name is not usable either: nothing
        // defaults to google, so the logged-out-looking profile loses to
        // the ready one.
        let mut keyed = rhema_config::Profile::default();
        store_key(&mut keyed, "google", "k");
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed),
                ("work".to_owned(), work_ready),
            ]),
            ..Default::default()
        };
        assert_eq!(
            startup_profile(&config, Path::new("/nowhere"), None),
            "work"
        );
    }

    #[test]
    fn a_workspace_reopens_on_the_profile_it_last_used() {
        let ready = |key: &str| keyed_profile("google", key);
        let mut config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), ready("k")),
                ("work".to_owned(), ready("w")),
            ]),
            ..Default::default()
        };
        // Both profiles can serve turns, and proj last ran on work: the
        // workspace memory wins over the personal default.
        config.note_profile_in(Path::new("/proj"), "work");
        assert_eq!(startup_profile(&config, Path::new("/proj"), None), "work");
        // Another directory has no memory: the default stands.
        assert_eq!(
            startup_profile(&config, Path::new("/other"), None),
            "personal"
        );
        // A memory naming a profile that can no longer serve turns falls
        // through to the default — never the login screen by surprise.
        config
            .profiles
            .insert("empty".to_owned(), rhema_config::Profile::default());
        config.note_profile_in(Path::new("/proj"), "empty");
        assert_eq!(
            startup_profile(&config, Path::new("/proj"), None),
            "personal"
        );
    }

    /// A profile whose `sandbox` section stores exactly this kind.
    fn sandbox_kind_profile(kind: &str) -> rhema_config::Profile {
        rhema_config::Profile {
            sandbox: Some(rhema_config::SandboxSettings {
                kind: Some(kind.to_owned()),
                ..rhema_config::SandboxSettings::default()
            }),
            ..rhema_config::Profile::default()
        }
    }

    /// What this machine offers, spelled per test.
    fn offered(
        docker: bool,
        lxd: bool,
        ecrou: bool,
        ssh: bool,
        http: bool,
    ) -> crate::sandbox::SandboxAvailability {
        crate::sandbox::SandboxAvailability {
            docker,
            lxd,
            ecrou,
            ssh,
            http,
        }
    }

    #[test]
    fn startup_sandbox_prefers_the_stored_kind_while_it_is_offered() {
        let config = Config {
            profiles: BTreeMap::from([(
                rhema_config::DEFAULT_PROFILE.to_owned(),
                sandbox_kind_profile("docker"),
            )]),
            ..Default::default()
        };
        let profile = config.effective(None);
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Docker
        );
        // The env-wins tier is injected the way the resolve tests inject
        // it: as the layered value `sandbox_kind` would have returned
        // with RHEMA_SANDBOX set (env over a stored local).
        let layered = sandbox_kind_profile("http");
        assert_eq!(
            startup_sandbox(
                &config,
                &layered,
                Path::new("/proj"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Http
        );
        // A spelling nothing answers to reads as unset: local.
        let typo = sandbox_kind_profile("dockre");
        assert_eq!(
            startup_sandbox(
                &config,
                &typo,
                Path::new("/proj"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Local
        );
    }

    #[test]
    fn startup_sandbox_falls_through_an_unoffered_stored_kind_to_the_remembered_one() {
        let mut config = Config {
            profiles: BTreeMap::from([("default".to_owned(), sandbox_kind_profile("docker"))]),
            ..Default::default()
        };
        config.note_sandbox_in(Path::new("/proj"), "ssh");
        let profile = config.effective(None);
        // Docker is not offered here; the workspace's remembered ssh is.
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(false, true, false, true, false)
            ),
            SandboxChoice::Ssh
        );
        // The remembered kind is offered nowhere either: local, and the
        // stored docker kind never stands a failed container up.
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(false, true, false, false, false)
            ),
            SandboxChoice::Local
        );
    }

    #[test]
    fn startup_sandbox_reads_the_workspace_memory_only_when_offered() {
        let mut config = Config::default();
        config.note_sandbox_in(Path::new("/proj"), "http");
        let profile = rhema_config::Profile::default();
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(false, true, false, false, true)
            ),
            SandboxChoice::Http
        );
        // A memory of a kind no machine answers to, and an unparseable
        // one besides, both fall through to local.
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(false, true, false, false, false)
            ),
            SandboxChoice::Local
        );
        config.note_sandbox_in(Path::new("/proj"), "nonsense");
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Local
        );
        // Another workspace has no memory at all.
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/other"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Local
        );
    }

    #[test]
    fn zero_config_starts_local_even_on_a_machine_that_offers_everything() {
        let config = Config::default();
        assert_eq!(
            startup_sandbox(
                &config,
                &rhema_config::Profile::default(),
                Path::new("/proj"),
                &offered(true, true, false, true, true)
            ),
            SandboxChoice::Local
        );
    }

    #[test]
    fn startup_sandbox_reads_the_real_env_through_sandbox_kind() {
        // The profile path runs only when the suite exports nothing:
        // the real environment decides, and the pure test above carries
        // the env-wins coverage (the edition forbids writing the var).
        if std::env::var_os("RHEMA_SANDBOX").is_some() {
            return;
        }
        let config = Config {
            profiles: BTreeMap::from([(
                rhema_config::DEFAULT_PROFILE.to_owned(),
                sandbox_kind_profile("docker"),
            )]),
            ..Default::default()
        };
        let profile = config.effective(None);
        assert_eq!(
            startup_sandbox(
                &config,
                &profile,
                Path::new("/proj"),
                &offered(true, true, false, false, false)
            ),
            SandboxChoice::Docker
        );
    }

    #[test]
    fn choosing_a_sandbox_stores_the_kind_notes_the_workspace_and_rebuilds() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // The session runs on docker (as an assembled startup would
        // have left it): switching to local must drop the assembly,
        // store the kind, remember the workspace, and rebuild inline.
        h.app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: SandboxChoice::Docker,
            target: "rhema-sandbox-proj".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        });
        h.app.sandbox_offered = offered(true, true, false, false, false);
        let effects = h.app.choose_sandbox(SandboxChoice::Local);
        assert!(effects.is_empty(), "local rebuilds inline: {effects:?}");
        assert!(h.app.sandboxed.is_none());
        assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
        let stored = h.store.load().unwrap();
        assert_eq!(
            stored.effective(Some("personal")).sandbox_kind().as_deref(),
            Some("local"),
            "the kind persists on the profile's sandbox section"
        );
        assert_eq!(
            stored.last_sandbox_in(Path::new(".")).as_deref(),
            Some("local"),
            "the workspace remembers the kind"
        );
    }

    #[test]
    fn choosing_a_non_local_sandbox_assembles_through_an_effect() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(true, true, false, false, false);
        let effects = h.app.choose_sandbox(SandboxChoice::Docker);
        assert_eq!(
            effects,
            vec![Effect::AssembleSandbox(SandboxChoice::Docker)]
        );
        assert_eq!(h.app.assembling, Some(SandboxChoice::Docker));
        // The choice is already durable: a crash before the assembly
        // lands must not lose it.
        let stored = h.store.load().unwrap();
        assert_eq!(
            stored.effective(Some("personal")).sandbox_kind().as_deref(),
            Some("docker")
        );
        assert_eq!(
            stored.last_sandbox_in(Path::new(".")).as_deref(),
            Some("docker")
        );
        // The assembly's answer swaps the tools in and rebuilds.
        let assembled = crate::sandbox::Sandboxed {
            choice: SandboxChoice::Docker,
            target: "rhema-sandbox-proj".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/host/ws")),
            workspace: "/host/ws".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        };
        let effects = h.app.on_event(crate::events::UiEvent::SandboxAssembled {
            choice: SandboxChoice::Docker,
            result: Ok(assembled),
        });
        assert!(effects.is_empty());
        assert_eq!(h.app.assembling, None);
        assert_eq!(h.app.active_sandbox(), SandboxChoice::Docker);
        assert!(
            h.app
                .agent
                .as_ref()
                .expect("the agent is rebuilt")
                .system()
                .is_some_and(|prompt| prompt.contains("/host/ws")),
            "the prompt names the sandbox-side workspace"
        );
    }

    #[test]
    fn a_failed_assembly_reports_in_band_and_keeps_the_running_tools() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(false, true, false, false, true);
        assert_eq!(
            h.app.choose_sandbox(SandboxChoice::Http),
            vec![Effect::AssembleSandbox(SandboxChoice::Http)]
        );
        h.app.on_event(crate::events::UiEvent::SandboxAssembled {
            choice: SandboxChoice::Http,
            result: Err("connection refused".to_owned()),
        });
        assert_eq!(h.app.assembling, None);
        assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
        // The transcript says what failed; the agent never dropped.
        let notice = h
            .app
            .transcript
            .lines()
            .iter()
            .rev()
            .find_map(|line| match line {
                crate::transcript::Line::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(notice.contains("http"), "{notice}");
        assert!(notice.contains("connection refused"), "{notice}");
        assert!(h.app.agent.is_some());
    }

    #[test]
    fn a_sandbox_this_machine_does_not_offer_is_declined_in_band() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = crate::sandbox::SandboxAvailability::default();
        assert!(h.app.choose_sandbox(SandboxChoice::Docker).is_empty());
        assert!(h.app.sandboxed.is_none());
        // The stored kind is untouched, and nothing was remembered.
        let stored = h.store.load().unwrap();
        assert_eq!(stored.effective(Some("personal")).sandbox_kind(), None);
        assert_eq!(stored.last_sandbox_in(Path::new(".")), None);
    }

    #[test]
    fn an_unknown_sandbox_spelling_is_declined_in_band() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/sandbox dockre");
        let stored = h.store.load().unwrap();
        assert_eq!(stored.effective(Some("personal")).sandbox_kind(), None);
    }

    #[test]
    fn slash_sandbox_with_an_instance_name_runs_bash_in_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(true, true, false, true, false);
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
        });
        h.store.save(&h.app.config).unwrap();
        let effects = crate::commands::dispatch(&mut h.app, "/sandbox dev");
        assert_eq!(
            effects,
            vec![Effect::AssembleSandbox(SandboxChoice::Docker)],
            "the instance's kind is what assembles"
        );
        let stored = h.store.load().unwrap();
        let sandbox = stored
            .effective(Some("personal"))
            .sandbox
            .expect("the section lands");
        assert_eq!(sandbox.kind.as_deref(), Some("docker"));
        assert_eq!(
            sandbox.active.as_deref(),
            Some("dev"),
            "the pointer names the instance"
        );
    }

    #[test]
    fn slash_sandbox_with_a_kind_still_selects_the_kind() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(true, true, false, false, false);
        let effects = crate::commands::dispatch(&mut h.app, "/sandbox docker");
        assert_eq!(
            effects,
            vec![Effect::AssembleSandbox(SandboxChoice::Docker)]
        );
        let stored = h.store.load().unwrap();
        let sandbox = stored
            .effective(Some("personal"))
            .sandbox
            .expect("the section lands");
        assert_eq!(sandbox.kind.as_deref(), Some("docker"));
        assert_eq!(
            sandbox.active, None,
            "a kind select never points at an instance"
        );
    }

    #[test]
    fn an_unknown_sandbox_name_names_the_instances_and_the_kinds() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/sandbox nowhere");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("nowhere")
        ));
    }

    #[test]
    fn a_profile_switch_moves_bash_to_the_new_profiles_sandbox() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "o"));
        h.app.sandbox_machines.docker = true;
        h.app.sandbox_offered = offered(true, true, false, false, false);
        // The session runs on the current profile's docker instance.
        h.app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: SandboxChoice::Docker,
            target: "work-box".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        });
        // work (moving in) stores nothing: its own sandbox is local.
        let effects = h.app.switch_profile("work");
        assert!(h.app.sandboxed.is_none(), "the docker assembly leaves");
        assert_eq!(h.app.active_sandbox(), SandboxChoice::Local);
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::AssembleSandbox(_))),
            "local needs no assembly: {effects:?}"
        );
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("bash runs on this host")
        ));
    }

    #[test]
    fn a_profile_switch_assembles_the_profile_moving_in() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_machines.docker = true;
        h.app.sandbox_offered = offered(true, true, false, false, false);
        // The session runs local; work stores its own docker instance.
        let mut work = keyed_profile("openrouter", "o");
        work.sandbox = Some(rhema_config::SandboxSettings {
            kind: Some("docker".to_owned()),
            active: Some("work-box".to_owned()),
            ..rhema_config::SandboxSettings::default()
        });
        work.place_sandbox_instance(
            "work-box",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("work-box".to_owned()),
                vm: None,
            },
        );
        h.app.config.profiles.insert("work".to_owned(), work);
        let effects = h.app.switch_profile("work");
        assert_eq!(h.app.assembling, Some(SandboxChoice::Docker));
        assert_eq!(
            effects,
            vec![
                Effect::FetchUsage,
                Effect::AssembleSandbox(SandboxChoice::Docker)
            ],
            "the assembly effect rides the switch"
        );
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("standing up the docker sandbox")
        ));
    }

    #[test]
    fn a_switch_keeps_a_sandbox_the_new_profile_resolves_to() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_machines.docker = true;
        h.app.sandbox_offered = offered(true, true, false, false, false);
        h.app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: SandboxChoice::Docker,
            target: "work-box".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        });
        // work resolves to the same kind AND the same target: the
        // running assembly is already the profile's own sandbox.
        let mut work = keyed_profile("openrouter", "o");
        work.sandbox = Some(rhema_config::SandboxSettings {
            kind: Some("docker".to_owned()),
            active: Some("work-box".to_owned()),
            ..rhema_config::SandboxSettings::default()
        });
        work.place_sandbox_instance(
            "work-box",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some("work-box".to_owned()),
                vm: None,
            },
        );
        h.app.config.profiles.insert("work".to_owned(), work);
        let effects = h.app.switch_profile("work");
        assert!(h.app.sandboxed.is_some(), "the assembly stays");
        assert_eq!(h.app.assembling, None);
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::AssembleSandbox(_))),
            "no re-assembly for the same place: {effects:?}"
        );
    }

    #[test]
    fn the_offered_set_rederives_for_the_profile_moving_in() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // The ssh binary answers on this machine; personal names no
        // host, so ssh is not offered to it.
        h.app.sandbox_machines.ssh = true;
        h.app.sandbox_offered = offered(false, false, false, false, false);
        assert!(!h.app.sandbox_offered.ssh);
        let mut work = keyed_profile("openrouter", "o");
        work.sandbox = Some(rhema_config::SandboxSettings {
            host: Some("deploy@box".to_owned()),
            ..rhema_config::SandboxSettings::default()
        });
        h.app.config.profiles.insert("work".to_owned(), work);
        h.app.switch_profile("work");
        assert!(
            h.app.sandbox_offered.ssh,
            "work names a host: ssh is offered to it"
        );
    }

    #[test]
    fn the_sandbox_badge_names_the_kind_or_the_instance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        assert_eq!(h.app.sandbox_badge(), "local", "local is a fact too");
        h.app.sandboxed = Some(crate::sandbox::Sandboxed {
            choice: SandboxChoice::Docker,
            target: "work-box".to_owned(),
            tools: std::sync::Arc::new(rhema_coding_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
        });
        // No instance stored: the badge is the kind.
        assert_eq!(h.app.sandbox_badge(), "docker");
        // The active pointer names the instance the kind resolves to:
        // the badge says where, not just what.
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.sandbox = Some(rhema_config::SandboxSettings {
                kind: Some("docker".to_owned()),
                active: Some("dev".to_owned()),
                ..rhema_config::SandboxSettings::default()
            });
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
        });
        assert_eq!(h.app.sandbox_badge(), "dev");
    }

    #[test]
    fn profile_sandbox_reads_the_profile_alone() {
        // No stored kind reads local, even where the workspace
        // remembers another profile's docker: a switch never inherits.
        let everything = offered(true, true, false, true, true);
        assert_eq!(
            profile_sandbox(&rhema_config::Profile::default(), &everything),
            SandboxChoice::Local
        );
        // The stored kind wins while the machine offers it...
        let stored = sandbox_kind_profile("ssh");
        assert_eq!(profile_sandbox(&stored, &everything), SandboxChoice::Ssh);
        // ...and an unoffered kind falls to local, never to a memory.
        assert_eq!(
            profile_sandbox(&stored, &offered(false, false, false, false, false)),
            SandboxChoice::Local
        );
    }

    #[test]
    fn slash_sandbox_opens_the_settings_sandbox_page() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(true, true, false, true, false);
        crate::commands::dispatch(&mut h.app, "/sandbox");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Sandbox);
        // An empty config: the host's own row and the add row — the
        // default view is only local.
        assert_eq!(
            settings
                .sandbox_rows()
                .iter()
                .map(|row| (row.name, row.offered))
                .collect::<Vec<_>>(),
            vec![("local", true), ("", false)]
        );
    }

    #[test]
    fn enter_on_a_sandbox_instance_row_runs_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.sandbox_offered = offered(true, true, false, true, false);
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
        });
        h.store.save(&h.app.config).unwrap();
        crate::commands::dispatch(&mut h.app, "/sandbox");
        // Down to the dev row, then enter: the assembly effect fires
        // and the page closes.
        h.app.on_key(key(KeyCode::Down));
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::AssembleSandbox(SandboxChoice::Docker)],
            "enter on the dev instance assembles its kind"
        );
        assert!(matches!(h.app.screen, Screen::Chat), "enter returns");
        let stored = h.store.load().unwrap();
        let profile = stored.effective(Some("personal"));
        let sandbox = profile.sandbox.as_ref().expect("the section lands");
        assert_eq!(sandbox.kind.as_deref(), Some("docker"));
        assert_eq!(
            sandbox.active.as_deref(),
            Some("dev"),
            "the pointer names the instance"
        );
    }

    #[test]
    fn sandbox_d_removes_the_instance_and_keeps_the_kind() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // A stored instance to remove, active and all.
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
            profile.sandbox = Some(rhema_config::SandboxSettings {
                kind: Some("docker".to_owned()),
                active: Some("dev".to_owned()),
                container: Some("rhema-dev".to_owned()),
                ..rhema_config::SandboxSettings::default()
            });
        });
        h.store.save(&h.app.config).unwrap();
        crate::commands::dispatch(&mut h.app, "/sandbox");
        // One down to the dev row, then d.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Char('d')));
        let stored = h.store.load().unwrap();
        let profile = stored.effective(Some("personal"));
        assert!(
            !profile.sandbox_instances.contains_key("dev"),
            "the instance is gone from the map"
        );
        let sandbox = profile.sandbox.as_ref().expect("the section stays");
        assert_eq!(
            sandbox.active, None,
            "the active pointer went with the instance"
        );
        assert_eq!(
            sandbox.kind.as_deref(),
            Some("docker"),
            "the kind stays put"
        );
        assert!(
            h.app
                .transcript
                .lines()
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("removed"))),
            "the notice says what happened"
        );
        // d on local does nothing: there is no instance there.
        h.app.on_key(key(KeyCode::Up));
        let notices = h.app.transcript.lines().len();
        h.app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            h.app.transcript.lines().len(),
            notices,
            "no notice: d on local is inert"
        );
    }

    #[test]
    fn the_sandbox_add_dialog_stores_the_kinds_instance() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/sandbox");
        // One down: local, add row. Enter opens the dialog; enter
        // again stores docker — no name asked, the concrete one is
        // the workspace's to set at assembly time.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter)); // store docker
        let stored = h.store.load().unwrap();
        let profile = stored.effective(Some("personal"));
        let instance = profile
            .sandbox_instances
            .get("docker")
            .expect("the instance stored under the kind's name");
        assert_eq!(instance.kind, "docker");
        assert_eq!(
            instance.target, None,
            "no target collected: derived per workspace"
        );
        let sandbox = profile.sandbox.as_ref();
        assert!(
            sandbox
                .map(|s| s.kind.is_none() && s.active.is_none())
                .unwrap_or(true),
            "storing never activates: the kind and the pointer stay unset"
        );
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("still on the settings screen");
        };
        assert!(settings.sandbox_dialog.is_none(), "the dialog closed");
        assert!(
            h.app
                .transcript
                .lines()
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("docker"))),
            "the notice names the instance"
        );
        // Same flow, lxd picked — two downs now, docker's row sits
        // between local and the add row. Its launch flag stays unset
        // (config.yaml's call).
        crate::commands::dispatch(&mut h.app, "/sandbox");
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter)); // store lxd
        let stored = h.store.load().unwrap();
        let profile = stored.effective(Some("personal"));
        let instance = profile
            .sandbox_instances
            .get("lxd")
            .expect("the lxd instance stored");
        assert_eq!(instance.kind, "lxd");
        assert_eq!(instance.target, None);
        assert_eq!(instance.vm, None, "the instance type stays config's");
    }

    #[test]
    fn the_sandbox_palette_completes_the_instances() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.place_sandbox_instance(
                "dev",
                rhema_config::SandboxInstance {
                    kind: "docker".to_owned(),
                    target: Some("rhema-dev".to_owned()),
                    vm: None,
                },
            );
            profile.place_sandbox_instance(
                "build",
                rhema_config::SandboxInstance {
                    kind: "lxd".to_owned(),
                    target: None,
                    vm: Some(true),
                },
            );
        });
        let sandboxes = h.app.palette_sandboxes();
        let rows = |input: &str| {
            crate::screens::palette_rows(
                input,
                input.chars().count(),
                &[],
                &[],
                &sandboxes,
                &crate::icons::UNICODE,
            )
        };
        // The empty argument lists the configured instances and
        // nothing else: the bare kinds never join the rows.
        let names: Vec<String> = rows("/sandbox ")
            .into_iter()
            .filter_map(|row| match row {
                crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
                _ => None,
            })
            .collect();
        // Map order: sandbox_instances is a BTreeMap, alphabetical.
        assert_eq!(names, ["build".to_owned(), "dev".to_owned()]);
        // A stem narrows across the name, the kind, and the target.
        assert_eq!(
            rows("/sandbox dev")
                .into_iter()
                .filter_map(|row| match row {
                    crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["dev".to_owned()],
            "the instance name matches"
        );
        assert_eq!(
            rows("/sandbox rhema")
                .into_iter()
                .filter_map(|row| match row {
                    crate::screens::PaletteRow::Sandbox { value, .. } => Some(value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["dev".to_owned()],
            "the target matches too"
        );
        // Tab fills the instance's name under the command prefix.
        let mut chat = crate::screens::ChatState::new();
        for c in "/sandbox dev".chars() {
            chat.handle_key(
                key(KeyCode::Char(c)),
                &[],
                &[],
                &sandboxes,
                &crate::icons::UNICODE,
            );
        }
        chat.handle_key(
            key(KeyCode::Tab),
            &[],
            &[],
            &sandboxes,
            &crate::icons::UNICODE,
        );
        assert_eq!(chat.input, "/sandbox dev");
    }

    #[test]
    fn slash_thinking_persists_the_level_and_rebuilds_the_agent() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/thinking high");
        assert_eq!(
            h.app.options.thinking,
            rhema_core::request::ThinkingEffort::High
        );
        let stored = h.store.load().unwrap();
        let profile = h.app.session_profile.clone().unwrap_or_default();
        assert_eq!(
            stored.effective(Some(&profile)).thinking.as_deref(),
            Some("high")
        );
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("thinking effort set to high".into()))
        );

        // An unknown level is declined in-band, and default stores its
        // name: the config never carries a bare null, which would read
        // as medium on the next load.
        crate::commands::dispatch(&mut h.app, "/thinking maximal");
        assert!(h.app.error.is_none());
        crate::commands::dispatch(&mut h.app, "/thinking default");
        assert_eq!(
            h.app.options.thinking,
            rhema_core::request::ThinkingEffort::Default
        );
        assert_eq!(
            h.store.load().unwrap().effective(Some(&profile)).thinking,
            Some("default".to_owned())
        );
    }

    #[test]
    fn a_turn_records_its_profile_for_the_workspace() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let workspace = h.app.options.workspace.clone();
        h.app.session_profile = Some("work".to_owned());
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        h.app
            .begin_turn(cancel, tokio::sync::mpsc::channel(STEER_QUEUE_CAP).0, None);
        assert_eq!(
            h.app.config.last_profile_in(&workspace).as_deref(),
            Some("work"),
            "the workspace remembers the profile"
        );
        // What the next launch reads is the saved file, not live state.
        assert_eq!(
            h.store
                .load()
                .unwrap()
                .last_profile_in(&workspace)
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn a_logged_in_profile_opens_the_chat_without_the_login_screen() {
        // The run_tui glue: resolve the no-flag profile, build the
        // backend from its section, and hand both to App::new. personal
        // is empty, work is ready with a model.
        let config = Config {
            profiles: BTreeMap::from([
                ("personal".to_owned(), rhema_config::Profile::default()),
                (
                    "work".to_owned(),
                    rhema_config::Profile {
                        model: Some("work-model".into()),
                        ..keyed_profile("openrouter", "work-key")
                    },
                ),
            ]),
            ..Default::default()
        };
        let profile = startup_profile(&config, Path::new("/nowhere"), None);
        let backend = Backend::from_profile(&config.effective(Some(&profile)), None);
        let app = App::new(AppSetup {
            store: Arc::new(MemoryStore::new(config.clone())),
            checkpoint: None,
            config,
            backend,
            session: Session::default(),
            secrets: None,
            sandboxed: None,
            sandbox_machines: crate::sandbox::SandboxMachines::default(),
            home: Some(std::env::temp_dir().join(format!("rhema-app-home-{}", std::process::id()))),
            onboard: false,
            options: RunOptions {
                colors: crate::palette::Palette::Rhema,
                theme: crate::palette::ThemeKind::Dark,
                m3_source: None,
                custom: None,
                thinking: rhema_core::request::ThinkingEffort::Default,
                icons: crate::icons::Flavor::Unicode,
                profile: Some(profile),
                workspace: PathBuf::from("."),
                max_steps: 4,
                no_transcript: false,
                base_system: None,
                save: None,
            },
        })
        .unwrap();
        assert!(matches!(app.screen, Screen::Chat), "no login screen");
        assert_eq!(app.session_profile.as_deref(), Some("work"));
        // The session reads work's section, so the footer shows its model.
        assert_eq!(app.effective().model.as_deref(), Some("work-model"));
    }

    #[test]
    fn slash_settings_opens_the_live_providers_page() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Providers);
        assert_eq!(settings.profile, "personal");
        assert_eq!(settings.stored.provider_name().as_deref(), Some("google"));
        assert_eq!(settings.stored.model.as_deref(), Some("m-a"));
        let rows = settings.instance_rows();
        assert_eq!(rows.len(), 2, "the google instance plus the add row");
        assert!(!rows[0].add);
        assert!(rows[0].active, "the only instance is the serving one");
        assert!(rows[1].add);
        assert!(!settings.workspace.is_empty(), "the About page names it");
    }

    #[test]
    fn slash_subagents_opens_the_definitions_page() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/subagents");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Subagents);
        // The catalog is never empty: the built-in is always a row, with
        // no file to open until the first edit copies it out.
        let builtin = settings
            .subagents
            .iter()
            .find(|row| row.name == "general-purpose")
            .expect("the built-in row");
        assert_eq!(builtin.source, SubagentSource::Builtin);
        assert_eq!(builtin.path, None);
        assert!(
            !settings.subagents_workspace_dir.is_empty(),
            "the page names the workspace directory"
        );
    }

    #[test]
    fn subagent_enter_on_a_definition_emits_the_edit_effect() {
        let workspace = tempfile::tempdir().unwrap();
        let folder = workspace
            .path()
            .join(".rhema")
            .join("subagents")
            .join("reviewer");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SUBAGENT.md"),
            "---\ndescription: reviews diffs\n---\n\nReview the diff.\n",
        )
        .unwrap();
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.options.workspace = workspace.path().to_path_buf();
        crate::commands::dispatch(&mut h.app, "/subagents");
        // The rows sort by name: the built-in first, the definition
        // second. The path is discovery's own — the canonicalized
        // workspace, since the opener canonicalizes.
        let file = std::fs::canonicalize(workspace.path())
            .unwrap()
            .join(".rhema")
            .join("subagents")
            .join("reviewer")
            .join("SUBAGENT.md");
        h.app.on_key(key(KeyCode::Down));
        assert_eq!(
            h.app.on_key(key(KeyCode::Enter)),
            vec![Effect::EditSubagent(file)]
        );
    }

    #[test]
    fn a_refresh_after_the_editor_picks_up_what_it_changed() {
        let workspace = tempfile::tempdir().unwrap();
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.options.workspace = workspace.path().to_path_buf();
        crate::commands::dispatch(&mut h.app, "/subagents");
        // The edit happens in $EDITOR, behind the app's back...
        let folder = workspace
            .path()
            .join(".rhema")
            .join("subagents")
            .join("reviewer");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SUBAGENT.md"),
            "---\ndescription: reviews diffs\n---\n\nReview the diff.\n",
        )
        .unwrap();
        // ...and the runner asks for the rows back once the TUI
        // re-enters.
        h.app.refresh_subagents();
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        let reviewer = settings
            .subagents
            .iter()
            .find(|row| row.name == "reviewer")
            .expect("the definition written behind the app's back");
        assert_eq!(reviewer.source, SubagentSource::Workspace);
        assert!(reviewer.path.is_some(), "the row names the file to edit");
    }

    #[test]
    fn subagent_rows_carry_the_provenance_discovery_shadows_by() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global_folder = global.path().join("reviewer");
        std::fs::create_dir_all(&global_folder).unwrap();
        std::fs::write(
            global_folder.join("SUBAGENT.md"),
            "---\ndescription: global trigger\n---\n\nBody.\n",
        )
        .unwrap();
        let workspace_folder = workspace
            .path()
            .join(".rhema")
            .join("subagents")
            .join("reviewer");
        std::fs::create_dir_all(&workspace_folder).unwrap();
        std::fs::write(
            workspace_folder.join("SUBAGENT.md"),
            "---\ndescription: workspace trigger\n---\n\nBody.\n",
        )
        .unwrap();

        let rows = subagent_rows(Some(global.path()), workspace.path());
        let reviewer = rows
            .iter()
            .find(|row| row.name == "reviewer")
            .expect("the shadowed definition");
        assert_eq!(
            reviewer.source,
            SubagentSource::Workspace,
            "the workspace shadows the global one"
        );
        assert_eq!(reviewer.description, "workspace trigger");
        assert_eq!(
            reviewer.path.as_deref(),
            Some(workspace_folder.join("SUBAGENT.md").as_path())
        );

        // Without the workspace file the same name reads as global.
        std::fs::remove_dir_all(&workspace_folder).unwrap();
        let rows = subagent_rows(Some(global.path()), workspace.path());
        let reviewer = rows.iter().find(|row| row.name == "reviewer").unwrap();
        assert_eq!(reviewer.source, SubagentSource::Global);
        assert_eq!(
            reviewer.path.as_deref(),
            Some(global_folder.join("SUBAGENT.md").as_path())
        );
    }

    #[test]
    fn shadowing_the_builtin_writes_a_discoverable_definition() {
        let global = tempfile::tempdir().unwrap();
        let file = shadow_builtin(global.path(), "general-purpose").unwrap();
        assert_eq!(
            file,
            global.path().join("general-purpose").join("SUBAGENT.md")
        );
        // What the copy wrote is a definition discovery accepts — the
        // shadow replaces the built-in wholesale, description and body.
        let workspace = tempfile::tempdir().unwrap();
        let defs = rhema_subagents::discover(Some(global.path()), workspace.path());
        assert_eq!(defs.len(), 1, "the shadow leaves nothing of the built-in");
        assert_eq!(defs[0].description, rhema_subagents::builtin().description);
        assert_eq!(defs[0].body, rhema_subagents::builtin().body);
        // An existing file is left alone: it is a shadow already.
        std::fs::write(&file, "---\ndescription: mine\n---\n\nMine.\n").unwrap();
        assert_eq!(
            shadow_builtin(global.path(), "general-purpose").unwrap(),
            file
        );
        let defs = rhema_subagents::discover(Some(global.path()), workspace.path());
        assert_eq!(defs[0].description, "mine");
    }

    #[test]
    fn settings_enter_on_the_add_row_opens_the_dialog() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Down to the add row, enter: the pop-in dialog comes up.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_some());
        // Esc closes the dialog, then leaves the settings for the chat.
        h.app.on_key(key(KeyCode::Esc));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.dialog.is_none());
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn settings_enter_on_an_instance_reactivates_it_and_lands_in_the_chat() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Enter on the highlighted instance re-activates it; with the
        // model stored, the session lands back in the chat.
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    /// `/settings` opens over a running turn without steering it: the
    /// model keeps working in the background, the screens read
    /// snapshots, and esc hands the still-running turn back.
    #[test]
    fn slash_settings_opens_over_a_running_turn_without_steering() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        for c in "/settings".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Settings(_)));
        assert!(h.app.turn.running);
        assert!(effects.is_empty(), "no steer, no turn start: {effects:?}");
        // The command never reached the transcript as a prompt.
        assert!(
            !h.app
                .transcript
                .lines()
                .iter()
                .any(|line| matches!(line, Line::User { .. }))
        );
        // Esc leaves the screen; the turn is still running underneath.
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(h.app.turn.running);
    }

    /// Every other command keeps its idle-only rule while a turn runs:
    /// the input is kept, nothing dispatches, nothing steers.
    #[test]
    fn other_commands_stay_inert_while_a_turn_runs() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        for c in "/model".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(h.app.chat.input, "/model");
    }

    #[test]
    fn settings_instance_activation_waits_for_a_running_turn() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.turn.running = true;
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(h.app.screen, Screen::Settings(_)),
            "the switch waits for an idle session"
        );
        // The screen itself still answers: esc leaves.
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn settings_toggles_the_theme_live_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        assert_eq!(h.app.theme, crate::palette::DARK);
        crate::commands::dispatch(&mut h.app, "/settings");
        // Three tabs: Profiles and Sandbox sit between Providers and
        // Appearance.
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::LIGHT, "the swap is live");
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
        // And back.
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::DARK);
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("dark"));
    }

    #[test]
    fn settings_toggles_the_icon_flavor_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('i')));
        assert_eq!(h.app.options.icons, crate::icons::Flavor::Nerd);
        assert!(
            std::ptr::eq(h.app.icons, crate::icons::Flavor::Nerd.set()),
            "the swap is live"
        );
        assert_eq!(h.store.load().unwrap().icons.as_deref(), Some("nerd"));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert!(settings.icons_nerd, "the page follows the toggle");
    }

    #[test]
    fn export_skips_a_session_with_nothing_to_resume() {
        // A fresh session saves nothing: there is no conversation.
        let h = harness(None, None, Session::default());
        assert_eq!(h.app.export_session(), None);

        // A session of notices alone (a switch, a decline) saves nothing
        // either: the history, not the transcript, decides.
        let mut h = harness(None, None, Session::default());
        h.app.push_notice("switched to profile work");
        assert_eq!(h.app.export_session(), None);

        // One prompt in: the live session exports, with no origin file.
        let mut h = harness(Some("k"), None, Session::default());
        h.app
            .session
            .history
            .push(Message::text(Role::User, "fix it"));
        let (exported, origin) = h.app.export_session().expect("a spoken session exports");
        assert_eq!(exported.history.len(), 1);
        assert_eq!(origin, None);
    }

    #[test]
    fn slash_resume_opens_a_picker_and_picking_resumes() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        // Two saved sessions for this workspace, newest first.
        let base = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(&h.app.options.workspace).unwrap();
        let older =
            crate::sessions::save_session(base.path(), &workspace, &session_of("the older prompt"))
                .unwrap()
                .unwrap();
        let newer =
            crate::sessions::save_session(base.path(), &workspace, &session_of("the newer prompt"))
                .unwrap()
                .unwrap();
        // Speak one line first: resuming replaces it.
        h.app.transcript.push_user("current talk", Vec::new());

        // The picker over the temp base; the dispatch path resolves its
        // own base from the environment, which a test cannot own.
        h.app.open_resume_from(base.path());
        match &h.app.screen {
            Screen::Resume(resume) => {
                assert_eq!(resume.entries.len(), 2);
                assert_eq!(resume.entries[0].path, newer, "newest first");
                assert_eq!(
                    resume.entries[0].snippet.as_deref(),
                    Some("the newer prompt")
                );
            }
            _other => panic!("expected the resume picker"),
        }

        // Down moves to the older session; enter loads it.
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(
            h.app
                .session
                .history
                .first()
                .and_then(|m| match m.content.first() {
                    Some(rhema_core::Content::Text(text)) => Some(text.as_str()),
                    _ => None,
                }),
            Some("the older prompt"),
            "the older session is the live one"
        );
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.app.transcript.lines().first(),
            Some(&Line::User {
                text: "the older prompt".into(),
                images: Vec::new()
            })
        );
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.starts_with("resumed ")
        ));
        // The session is now bound to the file it came from: exiting
        // updates it in place instead of listing it twice.
        assert_eq!(h.app.session_file.as_deref(), Some(older.as_path()));
        let (exported, origin) = h.app.export_session().expect("a spoken session exports");
        assert_eq!(origin.as_deref(), Some(older.as_path()));
        assert_eq!(exported.history.len(), 1);
        // The file the picker named is what got loaded.
        assert!(older.is_file() && newer.is_file());
    }

    #[test]
    fn the_startup_checkpoint_names_the_session_file() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let checkpoint =
            std::env::temp_dir().join(format!("rhema-checkpoint-{}-a.json", std::process::id()));
        h.app.session_file = Some(checkpoint.clone());

        // A fresh run's turns checkpoint into the allocated file.
        assert_eq!(h.app.checkpoint_target(), Some(checkpoint.clone()));
        // An explicit --save outranks it, for turns and for the exit
        // save alike.
        h.app.options.save = Some(PathBuf::from("/tmp/explicit-save.json"));
        assert_eq!(
            h.app.checkpoint_target(),
            Some(PathBuf::from("/tmp/explicit-save.json"))
        );
    }

    #[test]
    fn clear_drops_the_session_file_so_the_next_conversation_owns_its_own() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.session_file = Some(PathBuf::from("/tmp/cleared.json"));
        crate::commands::dispatch(&mut h.app, "/clear");
        assert_eq!(h.app.session_file, None);
        assert_eq!(h.app.checkpoint_target(), None);
    }

    #[test]
    fn slash_resume_with_nothing_saved_is_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let base = tempfile::tempdir().unwrap();
        h.app.open_resume_from(base.path());
        assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("no saved sessions")
        ));
    }

    fn session_of(prompt: &str) -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, prompt));
        session
    }

    #[test]
    fn slash_new_starts_a_fresh_session() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .session
            .history
            .push(Message::text(Role::User, "talk"));
        h.app.transcript.push_user("talk", Vec::new());
        h.app.session_file = Some(PathBuf::from("/tmp/old.json"));

        crate::commands::dispatch(&mut h.app, "/new");

        // A fresh session: no history to save, no resume binding, the
        // transcript down to the notice.
        assert_eq!(h.app.export_session(), None);
        assert_eq!(h.app.session_file, None);
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("cleared")
        ));
    }

    #[test]
    fn slash_skills_opens_the_skills_page() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/skills");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Skills);
        // Esc returns to the chat like every other settings page.
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn slash_system_shows_the_prompt_in_a_fenced_block() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());

        crate::commands::dispatch(&mut h.app, "/system");

        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Markdown(text))
                if text.starts_with("```markdown\n") && text.ends_with("```")
        ));
        // An argument is declined like any no-argument command.
        crate::commands::dispatch(&mut h.app, "/system now");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("unknown")
        ));
    }

    #[test]
    fn model_choice_lands_in_the_session_profile() {
        // Started with no --profile (so personal), then switched to work:
        // the pick must follow the session, not the startup flag.
        let mut h = harness(Some("k"), None, Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), keyed_profile("openrouter", "work-key"));
        crate::commands::dispatch(&mut h.app, "/profile work");
        crate::commands::dispatch(&mut h.app, "/model");
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Refresh,
            result: loaded("google", models()),
        });
        // Enter picks the first row (the picker sorts by id): "m-a".
        h.app.on_key(key(KeyCode::Enter));
        // The pick persists into work's section; personal's stays empty.
        let stored = h.store.load().unwrap();
        assert_eq!(stored.effective(Some("work")).model.as_deref(), Some("m-a"));
        assert_eq!(stored.effective(Some("personal")).model, None);
        // The rebuilt session reads the same section back.
        assert_eq!(h.app.effective().model.as_deref(), Some("m-a"));
    }

    #[test]
    fn slash_profile_without_credentials_still_switches() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.profiles.insert(
            "work".to_owned(),
            rhema_config::Profile {
                provider: Some("openrouter".into()),
                ..rhema_config::Profile::default()
            },
        );
        crate::commands::dispatch(&mut h.app, "/profile work");
        // The identity moves even without a key...
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("no API key")
        ));
        // ...but the old provider goes with the old profile: no backend,
        // no agent, no model list.
        assert!(h.app.backend.is_none());
        assert!(h.app.provider.is_none());
        assert!(h.app.agent.is_none());
        assert_eq!(h.app.effective().model, None, "reads work's section");

        // A prompt on a keyless profile is declined in-band and the text
        // stays for later.
        h.app.chat.input = "hello".to_owned();
        h.app.chat.cursor = 5;
        assert!(h.app.on_key(key(KeyCode::Enter)).is_empty());
        assert!(h.app.chat.input == "hello", "the prompt is kept");
        assert!(!h.app.turn.running, "no fake turn starts");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("/login")
        ));

        // /model is declined too: there is no backend to fetch from.
        crate::commands::dispatch(&mut h.app, "/model");
        assert!(matches!(h.app.screen, Screen::Chat), "no picker opens");

        // And the way back rebuilds on the kept session.
        crate::commands::dispatch(&mut h.app, "/profile personal");
        assert!(h.app.backend.is_some());
        assert!(h.app.agent.is_some());
        assert_eq!(h.app.session_profile.as_deref(), Some("personal"));
    }

    #[test]
    fn slash_profile_alone_opens_the_picker() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/profile");
        match &h.app.screen {
            Screen::Profile(profile) => {
                assert_eq!(profile.entries.len(), 1);
                assert_eq!(profile.entries[0].name, "personal");
            }
            _ => panic!("expected the profile picker"),
        }
    }

    #[test]
    fn slash_model_opens_the_picker_and_esc_returns_to_the_chat() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/model");
        assert!(matches!(&h.app.screen, Screen::Picker(_)));
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn slash_login_opens_the_providers_page_and_esc_returns_without_quitting() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Providers);
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(!h.app.should_quit, "esc goes back, not out");
    }

    #[test]
    fn esc_on_the_wizard_s_welcome_quits() {
        let mut h = harness(None, None, Session::default());
        h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
            Vec::new(),
            "personal".into(),
            false,
            rhema_config::Profile::default(),
        )));
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.should_quit, "no config behind the wizard: esc quits");
        // Mid-wizard, Esc walks back instead: the finish step is the
        // only one whose back-move cannot pass the welcome step.
        let mut h = harness(None, None, Session::default());
        h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
            Vec::new(),
            "personal".into(),
            false,
            rhema_config::Profile::default(),
        )));
        h.app.on_key(key(KeyCode::Enter)); // welcome -> login
        h.app.on_key(key(KeyCode::Esc));
        assert!(
            !h.app.should_quit,
            "an abandoned wizard only ever marks itself unfinished"
        );
        assert!(h.app.config.onboarded != Some(true));
    }

    /// A wizard fresh from `onboard: true` in the setup.
    fn wizard_harness() -> Harness {
        let mut h = harness(None, None, Session::default());
        h.app.screen = Screen::Onboarding(Box::new(crate::onboarding::OnboardingScreen::new(
            Vec::new(),
            "personal".into(),
            false,
            h.app.effective(),
        )));
        h
    }

    #[test]
    fn the_wizard_walks_skip_to_finish_and_marks_the_config_onboarded() {
        let mut h = wizard_harness();
        let Screen::Onboarding(wizard) = &h.app.screen else {
            panic!("expected the wizard");
        };
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Welcome);

        // Welcome -> Login; skip the login (row 1), landing on Profile.
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Down));
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Onboarding(wizard) = &h.app.screen else {
            panic!("expected the wizard");
        };
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Profile);

        // Profile (name kept) -> Colors -> Skills -> Finish -> chat.
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Chat));
        assert_eq!(h.app.config.onboarded, Some(true));
        let stored = h.store.load().unwrap();
        assert_eq!(stored.onboarded, Some(true));
    }

    #[test]
    fn the_wizard_s_login_advances_to_its_own_model_step() {
        let mut h = wizard_harness();
        h.app.on_key(key(KeyCode::Enter)); // welcome -> login
        h.app.on_key(key(KeyCode::Enter)); // add a login -> the dialog
        // The first provider row, the suggested name, a key.
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Enter));
        h.app.on_key(key(KeyCode::Char('k')));
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::FetchModels(FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "k".into(),
                },
                instance: Some("google".into()),
            })],
            "the wizard validates exactly like the settings dialog"
        );
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "k".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        let Screen::Onboarding(wizard) = &h.app.screen else {
            panic!("expected the wizard");
        };
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Model);
        assert_eq!(wizard.models.len(), 2);
        // Pick the highlighted model: the agent builds and the step
        // lands on Profile.
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::FetchUsage]);
        assert!(h.app.agent.is_some());
        assert_eq!(
            h.store.load().unwrap().effective(None).model.as_deref(),
            Some("m-a")
        );
        let Screen::Onboarding(wizard) = &h.app.screen else {
            panic!("expected the wizard");
        };
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Profile);
        assert_eq!(wizard.chosen_model.as_deref(), Some("m-a"));
    }

    #[test]
    fn the_wizard_s_colors_step_previews_and_the_settings_skills_page_toggles() {
        let mut h = wizard_harness();
        // Walk to Colors and step one palette down: the live apply is
        // the app's business, and the config is its record.
        h.app.on_key(key(KeyCode::Enter)); // welcome -> login
        h.app.on_key(key(KeyCode::Down)); // the skip row
        h.app.on_key(key(KeyCode::Enter)); // -> profile
        h.app.on_key(key(KeyCode::Enter)); // -> colors
        h.app.on_key(key(KeyCode::Down));
        let expected = crate::palette::Palette::ALL[1];
        assert_eq!(h.app.options.colors, expected, "applied live");
        assert_eq!(
            h.app.config.colors.as_deref(),
            Some(expected.stored_name()),
            "and persisted"
        );
        assert_eq!(
            h.store.load().unwrap().colors,
            h.app.config.colors,
            "and persisted"
        );

        // A skills toggle from the settings page lands in the disabled
        // set and flips the row on screen.
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.config.skills_disabled.clear();
        h.app
            .open_settings_at(crate::screens::SettingsSection::Skills);
        if let Screen::Settings(settings) = &mut h.app.screen {
            settings.skills = vec![crate::screens::SkillRow {
                name: "unslop".into(),
                description: "trigger".into(),
                license: Some("MIT".into()),
                source: crate::screens::SkillSource::Bundled,
                enabled: true,
            }];
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(
            h.app.config.skills_disabled.contains("unslop"),
            "the toggle lands in the disabled set"
        );
        assert_eq!(
            h.store.load().unwrap().skills_disabled,
            h.app.config.skills_disabled
        );
        if let Screen::Settings(settings) = &h.app.screen {
            assert!(!settings.skills[0].enabled, "the row follows");
        }
        // Toggle back on: the set clears (and a bundled skill would
        // re-seed from the re-enable path).
        h.app.on_key(key(KeyCode::Enter));
        assert!(!h.app.config.skills_disabled.contains("unslop"));
    }

    #[test]
    fn the_profiles_page_adds_a_profile_through_its_dialog() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .open_settings_at(crate::screens::SettingsSection::Profiles);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert_eq!(settings.profiles.len(), 1, "the harness ships personal");
        // The add row is the row past the profiles.
        if let Screen::Settings(settings) = &mut h.app.screen {
            settings.row = settings.profiles.len();
        }
        h.app.on_key(key(KeyCode::Enter));
        if let Screen::Settings(settings) = &h.app.screen {
            assert!(settings.add_profile.is_some(), "the dialog opened");
        }
        for c in "side".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        assert!(h.app.config.profiles.contains_key("side"), "created");
        assert_eq!(
            h.app.config.profiles["side"],
            rhema_config::Profile::default(),
            "creation is not a switch: the profile stays empty"
        );
        assert!(
            h.store.load().unwrap().profiles.contains_key("side"),
            "and persisted"
        );
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert!(settings.add_profile.is_none(), "the dialog closed");
        assert_eq!(settings.profiles.len(), 2, "the snapshot follows");

        // A taken name refuses in the dialog and changes nothing.
        if let Screen::Settings(settings) = &mut h.app.screen {
            settings.row = settings.profiles.len();
        }
        h.app.on_key(key(KeyCode::Enter));
        for c in "side".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Enter));
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert!(settings.add_profile.is_some(), "the dialog stayed up");
        assert!(
            settings.add_profile.as_ref().unwrap().error.is_some(),
            "the taken name names the error"
        );
        assert_eq!(h.app.config.profiles.len(), 2, "nothing was overwritten");
    }

    #[test]
    fn d_removes_a_profile_the_session_is_not_on() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());
        h.app
            .config
            .last_profile_by_workspace
            .insert("/proj".to_owned(), "work".to_owned());
        h.app
            .open_settings_at(crate::screens::SettingsSection::Profiles);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert_eq!(settings.profiles.len(), 2, "personal and work, sorted");
        // Row 1 is work; the session runs personal.
        if let Screen::Settings(settings) = &mut h.app.screen {
            settings.row = 1;
        }
        h.app.on_key(key(KeyCode::Char('d')));
        assert!(!h.app.config.profiles.contains_key("work"));
        let stored = h.store.load().unwrap();
        assert!(stored.profiles.contains_key("personal"));
        assert!(!stored.profiles.contains_key("work"), "and persisted");
        assert!(
            !stored
                .last_profile_by_workspace
                .values()
                .any(|remembered| remembered == "work"),
            "the workspace memory forgets the name"
        );
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert_eq!(settings.profiles.len(), 1, "the snapshot follows");
        assert!(settings.row <= settings.profiles.len(), "inside the list");
    }

    #[test]
    fn the_active_profile_refuses_removal() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());
        h.app
            .open_settings_at(crate::screens::SettingsSection::Profiles);
        // Row 0 is personal — the profile this session runs on. Its
        // removal is also what keeps the registry from emptying.
        h.app.on_key(key(KeyCode::Char('d')));
        assert!(
            h.app.config.profiles.contains_key("personal"),
            "the session's profile stays"
        );
        assert!(
            h.store.load().unwrap().profiles.contains_key("personal"),
            "and the stored config never loses it"
        );
    }

    #[test]
    fn c_cycles_a_profile_color_through_the_wheel_and_persists() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .open_settings_at(crate::screens::SettingsSection::Profiles);
        // Row 0 is personal.
        h.app.on_key(key(KeyCode::Char('c')));
        assert_eq!(
            h.app.config.profiles["personal"].color.as_deref(),
            Some(crate::palette::PROFILE_COLORS[0]),
            "auto steps to the first wheel color"
        );
        assert_eq!(
            h.store.load().unwrap().profiles["personal"].color,
            h.app.config.profiles["personal"].color,
            "and persists"
        );
        for _ in 0..crate::palette::PROFILE_COLORS.len() {
            h.app.on_key(key(KeyCode::Char('c')));
        }
        assert_eq!(
            h.app.config.profiles["personal"].color, None,
            "the wheel wraps to auto"
        );
        // A hand-edited color the wheel does not spell rejoins at auto.
        h.app.config.set_for_profile(Some("personal"), |profile| {
            profile.color = Some("#12abCD".to_owned());
        });
        h.app.on_key(key(KeyCode::Char('c')));
        assert_eq!(h.app.config.profiles["personal"].color, None);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected settings");
        };
        assert_eq!(settings.profiles[0].color, None, "the swatch follows");
    }

    #[test]
    fn enter_on_a_profiles_row_switches_the_session() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app
            .config
            .profiles
            .insert("work".to_owned(), rhema_config::Profile::default());
        h.app
            .open_settings_at(crate::screens::SettingsSection::Profiles);
        if let Screen::Settings(settings) = &mut h.app.screen {
            settings.row = 1; // work, which holds no credentials
        }
        h.app.on_key(key(KeyCode::Enter));
        assert_eq!(h.app.session_profile.as_deref(), Some("work"));
        assert!(
            matches!(h.app.screen, Screen::Chat),
            "the switch lands back in the chat"
        );
        assert!(
            h.app.backend.is_none(),
            "work holds no key: the backend waits for /login"
        );
    }

    #[test]
    fn a_click_on_the_wizard_s_rail_jumps_to_the_step() {
        let mut h = wizard_harness();
        // Draw one frame so the renderer registers the rail rects,
        // then click the Finish row (the rail's last step).
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        let (row_x, row_y) = h
            .app
            .onboarding_hits
            .iter()
            .find_map(|(area, hit)| {
                matches!(
                    hit,
                    crate::app::OnboardingHit::RailStep(crate::onboarding::OnboardingStep::Finish)
                )
                .then_some((area.x, area.y))
            })
            .expect("the finish row is clickable");
        h.app.on_click(row_x, row_y);
        let Screen::Onboarding(wizard) = &h.app.screen else {
            panic!("expected the wizard");
        };
        assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Finish);
        // A click lands on the row's cell, not beside it: one off the
        // rect changes nothing.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        h.app.on_click(row_x + 40, row_y);
        if let Screen::Onboarding(wizard) = &h.app.screen {
            assert_eq!(wizard.step, crate::onboarding::OnboardingStep::Finish);
        }
    }

    #[test]
    fn a_rerun_shows_the_existing_configuration() {
        // A configured profile walks in: the welcome rail says what is
        // connected instead of starting from a blank slate.
        let mut h = wizard_harness();
        if let Screen::Onboarding(wizard) = &mut h.app.screen {
            let mut stored = rhema_config::Profile {
                provider: Some("google".into()),
                instance: Some("google".into()),
                model: Some("gemini-x".into()),
                ..rhema_config::Profile::default()
            };
            stored.upsert_instance(
                "google",
                rhema_config::ProviderInstance {
                    provider: "google".into(),
                    api_key: None,
                    secret_ref: None,
                    base_url: None,
                },
            );
            wizard.stored = stored;
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        let text: String = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| {
                        let cell = &terminal.backend().buffer()[(x, y)];
                        cell.symbol().to_owned()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("currently configured"),
            "the welcome step names the config: {text}"
        );
        assert!(text.contains("google (google)"), "{text}");
        assert!(text.contains("gemini-x"), "{text}");

        // The login step lists the held instance; the model step keeps
        // the stored model.
        h.app.on_click(0, 0); // no-op; walk with keys instead
        if let Screen::Onboarding(wizard) = &mut h.app.screen {
            wizard.go(crate::onboarding::OnboardingStep::Login);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        let text: String = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("google (google)"), "login lists it: {text}");
        assert!(text.contains("+ add a login"), "{text}");
    }

    #[test]
    fn a_click_on_the_sidebar_switches_sections_and_a_row_acts() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/settings");
        // Draw one frame so the renderer registers the hit rects, then
        // click the Appearance row (the sidebar's second section row).
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(&mut h.app, frame))
            .unwrap();
        let row_y = h
            .app
            .settings_hits
            .iter()
            .find_map(|(area, hit)| {
                matches!(
                    hit,
                    crate::app::SettingsHit::Sidebar(crate::screens::SettingsSection::Appearance)
                )
                .then_some(area.y)
            })
            .expect("the appearance row is clickable");
        h.app.on_click(4, row_y);
        let Screen::Settings(settings) = &h.app.screen else {
            panic!("expected the settings screen");
        };
        assert_eq!(settings.section, SettingsSection::Appearance);
        // A click on a toggle row acts: the theme flips and persists.
        let (row_x, row_y) = h
            .app
            .settings_hits
            .iter()
            .find_map(|(area, hit)| {
                matches!(hit, crate::app::SettingsHit::PageRow(1)).then_some((area.x, area.y))
            })
            .expect("the theme row is clickable");
        h.app.on_click(row_x, row_y);
        assert_eq!(h.app.theme, crate::palette::LIGHT);
        assert_eq!(h.store.load().unwrap().theme.as_deref(), Some("light"));
    }

    #[test]
    fn settings_cycles_the_palette_live_and_persists_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        assert_eq!(h.app.theme, crate::palette::DARK);
        crate::commands::dispatch(&mut h.app, "/settings");
        // Three tabs: Profiles and Sandbox sit between Providers and
        // Appearance.
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Tab));
        h.app.on_key(key(KeyCode::Char('c')));
        assert_eq!(
            h.app.options.colors,
            crate::palette::Palette::Aquamarine,
            "rhema steps to aquamarine"
        );
        assert_eq!(h.app.theme, crate::palette::AQUAMARINE_DARK, "live swap");
        assert_eq!(
            h.store.load().unwrap().colors.as_deref(),
            Some("aquamarine")
        );
        // The appearance carries over: the palette's light tokens show.
        h.app.on_key(key(KeyCode::Char('t')));
        assert_eq!(h.app.theme, crate::palette::AQUAMARINE_LIGHT);
        // Cycling wraps back to rhema through every palette.
        for expected in [
            crate::palette::Palette::BlackWhite,
            crate::palette::Palette::Dracula,
            crate::palette::Palette::CatppuccinFrappe,
            crate::palette::Palette::CatppuccinMacchiato,
            crate::palette::Palette::CatppuccinMocha,
            crate::palette::Palette::Nord,
            crate::palette::Palette::Gruvbox,
            crate::palette::Palette::Solarized,
            crate::palette::Palette::M3You,
            crate::palette::Palette::Custom,
            crate::palette::Palette::Rhema,
        ] {
            h.app.on_key(key(KeyCode::Char('c')));
            assert_eq!(h.app.options.colors, expected);
        }
        assert_eq!(h.app.theme, crate::palette::LIGHT);
        assert_eq!(h.store.load().unwrap().colors.as_deref(), Some("rhema"));
    }

    #[test]
    fn slash_login_success_chains_into_a_picker_that_knows_the_way_back() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/login");
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Login {
                backend: Backend::Google {
                    api_key: "k".into(),
                },
                instance: Some("google".into()),
            },
            result: loaded("google", models()),
        });
        assert!(matches!(&h.app.screen, Screen::Picker(_)));
        h.app.on_key(key(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Chat));
    }

    #[test]
    fn esc_while_entering_a_command_clears_it_without_quit_confirm() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        for c in "/clear".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        h.app.on_key(key(KeyCode::Esc));
        assert!(h.app.chat.input.is_empty());
        assert!(!h.app.confirm_quit, "esc cancels the command, not the app");
    }

    #[test]
    fn slash_model_during_a_turn_declines_with_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        crate::commands::dispatch(&mut h.app, "/model");
        assert!(matches!(h.app.screen, Screen::Chat));
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("waits for it to finish")
        ));
    }

    #[test]
    fn slash_help_prints_commands_without_calling_the_model() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_key(key(KeyCode::Char('/')));
        for c in "help".chars() {
            h.app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(h.app.on_key(key(KeyCode::Enter)), Vec::new());
        assert!(!h.app.turn.running, "no turn starts");
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("/clear")
        ));
    }

    #[test]
    fn slash_clear_forges_a_fresh_session() {
        let session = Session {
            history: vec![Message::text(Role::User, "earlier")],
            driver_state: serde_json::Value::Null,
            cost_usd: 0.0,
            usage_stats: serde_json::Value::Null,
        };
        let mut h = harness(Some("k"), Some("m-a"), session);
        crate::commands::dispatch(&mut h.app, "/clear");
        assert!(h.app.agent.unwrap().session().history.is_empty());
        assert!(
            h.app.transcript.lines().len() == 1,
            "only the notice remains"
        );
    }

    #[test]
    fn slash_exit_quits_and_unknown_commands_get_a_notice() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        crate::commands::dispatch(&mut h.app, "/nope");
        assert!(!h.app.should_quit);
        assert!(matches!(
            h.app.transcript.lines().last(),
            Some(Line::Notice(text)) if text.contains("unknown command /nope")
        ));
        crate::commands::dispatch(&mut h.app, "/exit");
        assert!(h.app.should_quit);
    }

    #[test]
    fn the_status_tracker_follows_the_turn_lifecycle() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let mut tracker = crate::status::Tracker::default();

        // At rest before anything runs.
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Idle
        ));

        // A turn runs: the step drives progress against the 4-step cap.
        h.app.turn.running = true;
        tracker.observe(&UiEvent::Engine(OwnedEvent::StepStart { iteration: 2 }));
        match tracker.status_for(&h.app) {
            crate::status::Status::Working {
                msg: None,
                progress: Some(progress),
            } => assert_eq!(progress, 50),
            _ => panic!("a running turn is working at its step fraction"),
        }

        // The latest tool call names the working state.
        tracker.observe(&UiEvent::Engine(OwnedEvent::ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
        }));
        match tracker.status_for(&h.app) {
            crate::status::Status::Working {
                msg: Some(msg),
                progress: Some(50),
            } => assert_eq!(msg, "bash"),
            _ => panic!("the tool call is the working state's msg"),
        }

        // A pending approval blocks ahead of the running state.
        h.app.turn.pending_approval = Some(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::Value::Null,
            provider_data: serde_json::Value::Null,
        });
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Blocked { .. }
        ));

        // The turn ends in failure: the error stands at the idle prompt,
        // first line only.
        let agent = h
            .app
            .agent
            .take()
            .expect("the keyed harness built an agent");
        h.app.turn.pending_approval = None;
        h.app.turn.running = false;
        tracker.observe(&UiEvent::TurnFinished {
            agent: Box::new(agent),
            result: Err("provider exploded\nwith detail".to_owned()),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        match tracker.status_for(&h.app) {
            crate::status::Status::Error { msg: Some(msg) } => {
                assert_eq!(msg, "provider exploded");
            }
            _ => panic!("a failed turn reads as an error at the idle prompt"),
        }

        // A later turn outranks the remembered failure and ends clean:
        // back to idle, not to the old error.
        h.app.turn.running = true;
        tracker.observe(&UiEvent::Engine(OwnedEvent::StepStart { iteration: 0 }));
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Working { .. }
        ));
        let agent = CodingAgent::new(
            Arc::new(GoogleProvider::new("k")),
            CodingAgentConfig::new("m-a", std::env::temp_dir()),
        )
        .unwrap();
        h.app.turn.running = false;
        tracker.observe(&UiEvent::TurnFinished {
            agent: Box::new(agent),
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(matches!(
            tracker.status_for(&h.app),
            crate::status::Status::Idle
        ));
    }
    /// A background subagent job pins its live line off the wire's
    /// registry: working, it spins with its name, job id, and phase;
    /// finished, the line leaves at once and the finish event names
    /// the outcome; the fetch still serves the cached report. The
    /// render reads the snapshot fresh, so the strip's reserve matches
    /// what is on screen.
    #[tokio::test]
    async fn a_background_subagent_job_pins_its_live_line() {
        use rhema_core::{EventSink, ProviderError};
        use rhema_subagents::{SubagentDef, SubagentTools};

        /// Never answers: a job parked in its model stream.
        struct Hanging;
        /// Answers one report and ends.
        struct Report;

        #[async_trait::async_trait]
        impl ModelProvider for Hanging {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                std::future::pending().await
            }
        }

        #[async_trait::async_trait]
        impl ModelProvider for Report {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                Ok(rhema_core::ModelOutput {
                    message: Message::text(Role::Assistant, "the report"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }

        fn start_call() -> ToolCall {
            ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({"name": "scout", "prompt": "look"}),
                ..ToolCall::default()
            }
        }

        fn fetch_call() -> ToolCall {
            ToolCall {
                id: "c2".into(),
                name: "subagent".into(),
                args: serde_json::json!({"id": "job-1"}),
                ..ToolCall::default()
            }
        }

        fn scout_tools(provider: Arc<dyn ModelProvider>) -> Arc<SubagentTools> {
            let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
            let def = SubagentDef {
                name: "scout".into(),
                description: "looks around".into(),
                model: None,
                max_steps: None,
                body: "Look.".into(),
            };
            Arc::new(SubagentTools::new(provider, vec![def], shell, "m"))
        }

        fn frame_text(app: &mut App) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
            terminal
                .draw(|frame| crate::ui::render(app, frame))
                .unwrap();
            (0..40)
                .map(|y| {
                    (0..80)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect()
        }

        let mut harness = harness(Some("key"), Some("m"), Session::default());

        // A running job: the line spins with its name, id, and phase,
        // even with no turn running — jobs outlive turns.
        let tools = scout_tools(Arc::new(Hanging));
        harness.app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            start_call(),
            &rhema_core::ToolContext::default(),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while harness
            .app
            .subagent_jobs()
            .first()
            .is_none_or(|job| job.phase == "queued")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let text = frame_text(&mut harness.app);
        assert!(text.contains("scout (job-1)"), "{text}");
        assert!(text.contains("thinking"), "{text}");

        // Finished: the line leaves the strip at once; the finish
        // event names the outcome.
        let tools = scout_tools(Arc::new(Report));
        harness.app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            start_call(),
            &rhema_core::ToolContext::default(),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let finished = loop {
            let events = harness.app.subagents.as_ref().unwrap().take_finished();
            if !events.is_empty() {
                break events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the job never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(finished[0].line, "Subagent scout (job-1) has finished.");
        let text = frame_text(&mut harness.app);
        assert!(
            !text.contains("scout (job-1)"),
            "a finished job is not pinned: {text}"
        );
        assert!(harness.app.subagent_jobs().is_empty());

        // The fetch still serves the cached report; the strip stays
        // empty.
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            fetch_call(),
            &rhema_core::ToolContext::default(),
        )
        .await;
        let text = frame_text(&mut harness.app);
        assert!(!text.contains("scout (job-1)"), "{text}");
    }

    /// A job finishing with no turn running runs its notification as
    /// the next turn's prompt — the regular-prompt path. The tick
    /// emits `StartTurn` with the notification line, the transcript
    /// records it as the user line it is, and the take retires the
    /// announcement: the driver's drain has nothing left to repeat,
    /// and a second tick starts nothing.
    #[tokio::test]
    async fn an_idle_session_runs_a_finished_jobs_notification_as_the_next_prompt() {
        use rhema_core::{EventSink, ProviderError};
        use rhema_subagents::{SubagentDef, SubagentTools};

        /// Answers one report and ends.
        struct Report;

        #[async_trait::async_trait]
        impl ModelProvider for Report {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                Ok(rhema_core::ModelOutput {
                    message: Message::text(Role::Assistant, "the report"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }

        let mut h = harness(Some("key"), Some("m"), Session::default());
        let tools = {
            let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
            let def = SubagentDef {
                name: "scout".into(),
                description: "looks around".into(),
                model: None,
                max_steps: None,
                body: "Look.".into(),
            };
            Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
        };
        h.app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({"name": "scout", "prompt": "look"}),
                ..ToolCall::default()
            },
            &rhema_core::ToolContext::default(),
        )
        .await;
        // A finished job left the snapshot; its finish event stays
        // pending for the tick.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while h
            .app
            .subagents
            .as_ref()
            .unwrap()
            .status()
            .iter()
            .any(|job| job.id == "job-1")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The idle tick: the notification becomes the prompt.
        let effects = h.app.on_tick();
        assert_eq!(effects.len(), 1);
        let Effect::StartTurn {
            text: prompt,
            images,
        } = &effects[0]
        else {
            panic!("the idle tick starts a turn, got {:?}", effects[0]);
        };
        assert_eq!(prompt, "Subagent scout (job-1) has finished.");
        assert!(images.is_empty());
        let last = h.app.transcript.lines().last();
        assert!(
            matches!(last, Some(Line::User { text, .. }) if text == prompt.as_str()),
            "the prompt's user line is the record, got {last:?}"
        );

        // A second tick starts nothing: the take retired the line.
        assert!(h.app.on_tick().is_empty());
        // The driver's drain is empty too — the prompt already said it.
        let steering = h.app.subagents.as_ref().unwrap().steering();
        assert!(steering.drain().is_empty(), "a taken line stops announcing");
    }

    /// A job finishing while a turn runs shows the notice and leaves
    /// the delivery to the driver's steering: the tick starts nothing
    /// and takes no notification line, so the drain still names the
    /// job.
    #[tokio::test]
    async fn a_running_turn_shows_the_finish_notice_and_leaves_steering_to_the_driver() {
        use rhema_core::{EventSink, ProviderError};
        use rhema_subagents::{SubagentDef, SubagentTools};

        /// Answers one report and ends.
        struct Report;

        #[async_trait::async_trait]
        impl ModelProvider for Report {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                Ok(rhema_core::ModelOutput {
                    message: Message::text(Role::Assistant, "the report"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }

        let mut h = harness(Some("key"), Some("m"), Session::default());
        let tools = {
            let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
            let def = SubagentDef {
                name: "reviewer".into(),
                description: "reviews".into(),
                model: None,
                max_steps: None,
                body: "Review.".into(),
            };
            Arc::new(SubagentTools::new(Arc::new(Report), vec![def], shell, "m"))
        };
        h.app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({"name": "reviewer", "prompt": "review"}),
                ..ToolCall::default()
            },
            &rhema_core::ToolContext::default(),
        )
        .await;
        h.app.turn.running = true;
        // A finished job left the snapshot; its finish event stays
        // pending for the tick.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while h
            .app
            .subagents
            .as_ref()
            .unwrap()
            .status()
            .iter()
            .any(|job| job.id == "job-1")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let effects = h.app.on_tick();
        assert!(
            effects.is_empty(),
            "a running turn takes no prompt: {effects:?}"
        );
        let last = h.app.transcript.lines().last();
        assert!(
            matches!(last, Some(Line::Notice(text))
                if text == "Subagent reviewer (job-1) has finished."),
            "the finish lands as a notice, got {last:?}"
        );
        // The driver still steers: the announcement line is pending.
        let steering = h.app.subagents.as_ref().unwrap().steering();
        assert_eq!(
            steering.drain(),
            vec!["Subagent reviewer (job-1) has finished.".to_owned()]
        );
    }

    /// A turn that failed with a network error leaves the offer up:
    /// the error strip rides the hint, and `/retry` re-issues the same
    /// request — no new prompt, the flag spent.
    #[test]
    fn a_failed_turn_offers_retry_and_retry_fires_it() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Err("network error: connection reset".to_owned()),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(h.app.retryable);
        assert_eq!(
            h.app.error.as_deref(),
            Some("network error: connection reset")
        );

        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert_eq!(effects, vec![Effect::RetryTurn]);
        assert!(h.app.turn.running, "the retry takes the turn slot");
        assert!(!h.app.retryable, "the offer is spent");
        assert!(h.app.error.is_none());
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("retrying".into())),
            "the notice marks the fresh output block"
        );
    }

    /// `/retry` restarts the subagent jobs the network killed, offer
    /// or no offer: a job can die to an outage the parent turn
    /// survived, and the notice names what came back. Nothing failed
    /// elsewhere, so no turn starts; a second dispatch has nothing
    /// left to restart and says so.
    #[tokio::test]
    async fn retry_restarts_transient_failed_subagents() {
        use rhema_core::{EventSink, ProviderError};
        use rhema_subagents::{SubagentDef, SubagentTools};

        /// Fails the first call with a dead link, then answers.
        struct Flaky(std::sync::atomic::AtomicBool);

        #[async_trait::async_trait]
        impl ModelProvider for Flaky {
            async fn complete(
                &self,
                _req: rhema_core::ModelRequest,
                _sink: &mut dyn EventSink,
            ) -> Result<rhema_core::ModelOutput, ProviderError> {
                if !self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    // Already spent: the link recovered.
                    return Ok(rhema_core::ModelOutput {
                        message: Message::text(Role::Assistant, "the report"),
                        stop_reason: StopReason::EndTurn,
                        usage: Usage::default(),
                    });
                }
                Err(ProviderError::Network(
                    "error decoding response body".into(),
                ))
            }
        }

        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let shell = Arc::new(rhema_coding_agent::ShellTools::new(std::env::temp_dir()));
        let def = SubagentDef {
            name: "scout".into(),
            description: "looks around".into(),
            model: None,
            max_steps: None,
            body: "Look.".into(),
        };
        let tools = Arc::new(
            SubagentTools::new(Arc::new(Flaky(true.into())), vec![def], shell, "m")
                .with_retry_ladders(vec![], vec![]),
        );
        h.app.subagents = Some(tools.handle());
        let _ = rhema_core::ToolResolver::resolve(
            tools.as_ref(),
            ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({"name": "scout", "prompt": "look"}),
                ..ToolCall::default()
            },
            &rhema_core::ToolContext::default(),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if h.app.subagents.as_ref().unwrap().status().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the job never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The failure notice drains on the tick a real frontend runs;
        // the test takes it by hand.
        let _ = h.app.subagents.as_ref().unwrap().take_finished();

        // The restart: no failed turn, so no turn, but the job comes
        // back under its own id and the notice names it.
        assert!(!h.app.retryable);
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert!(effects.is_empty(), "{effects:?}");
        assert!(
            h.app.transcript.lines().contains(&Line::Notice(
                "restarted 1 subagent after network failures: scout (job-1)".into()
            )),
            "{:?}",
            h.app.transcript.lines().last()
        );

        // The re-run answers: the finish announces into the same id.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let events = loop {
            let events = h.app.subagents.as_ref().unwrap().take_finished();
            if !events.is_empty() {
                break events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the re-run never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(events[0].line, "Subagent scout (job-1) has finished.");

        // Spent: a second dispatch is the plain no-offer answer.
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert!(effects.is_empty());
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("no failed turn to retry".into()))
        );

        // With a failed turn behind it as well, one dispatch restarts
        // the jobs and fires the turn — the whole outage rewound.
        h.app.retryable = true;
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert_eq!(effects, vec![Effect::RetryTurn]);
    }

    /// Everything that moves the conversation on clears the offer: a
    /// fresh submit, a finished turn, a cancel, and a cleared
    /// conversation each leave `/retry` with nothing to re-issue.
    #[test]
    fn the_retry_offer_clears_when_the_conversation_moves_on() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        let steer = tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1;

        // Nothing failed yet.
        assert!(!h.app.retryable);
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert!(effects.is_empty());
        assert!(
            h.app
                .transcript
                .lines()
                .contains(&Line::Notice("no failed turn to retry".into()))
        );

        // The failed turn raises it; the next submit spends it.
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Err("network error: connection reset".to_owned()),
            steer,
        });
        h.app.on_key(key(KeyCode::Char('a')));
        let effects = h.app.on_key(key(KeyCode::Enter));
        assert!(matches!(effects.as_slice(), [Effect::StartTurn { .. }]));
        assert!(!h.app.retryable);
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert!(effects.is_empty());

        // A finished turn's outcome rules the flag either way: success
        // clears, failure raises again.
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.retryable);

        // A cancel is a choice, not a failure.
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnCancelled {
            agent,
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        assert!(!h.app.retryable);
        assert!(!matches!(
            crate::commands::dispatch(&mut h.app, "/retry").as_slice(),
            [Effect::RetryTurn]
        ));

        // Clearing the conversation retires it too.
        h.app.retryable = true;
        h.app.clear_session();
        assert!(!h.app.retryable);
    }

    /// The offer survives an agent rebuild on purpose: a failed turn,
    /// then a model switch, then `/retry` re-issues the request on the
    /// new model — the rate-limit answer.
    #[test]
    fn a_model_switch_keeps_the_retry_offer() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.on_event(UiEvent::ModelsLoaded {
            why: FetchWhy::Bootstrap,
            result: loaded("google", models()),
        });
        let agent = Box::new(h.app.agent.take().unwrap());
        h.app.on_event(UiEvent::TurnFinished {
            agent,
            result: Err("provider api error: 429 (retry-after: 37)".to_owned()),
            steer: tokio::sync::mpsc::channel(STEER_QUEUE_CAP).1,
        });
        crate::commands::dispatch(&mut h.app, "/model m-b");
        assert!(h.app.agent.is_some(), "the pick rebuilt the agent");
        assert!(h.app.retryable, "the switch is not a conversation change");
        assert_eq!(
            crate::commands::dispatch(&mut h.app, "/retry"),
            vec![Effect::RetryTurn]
        );
    }

    /// While a turn runs — a retry included — `/retry` waits.
    #[test]
    fn retry_waits_while_a_turn_runs() {
        let mut h = harness(Some("k"), Some("m-a"), Session::default());
        h.app.turn.running = true;
        let effects = crate::commands::dispatch(&mut h.app, "/retry");
        assert!(effects.is_empty());
        assert!(h.app.transcript.lines().contains(&Line::Notice(
            "a turn is running; /retry waits for it to finish".into()
        )));
    }
