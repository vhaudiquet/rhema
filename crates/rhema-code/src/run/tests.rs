//! The runner's tests: exit codes, the system actions, and the
//! prompt-section gate.

use super::headless::failed_checkpoint_path;
use super::system::{
    decision_section, markdown_ansi, search_section, system_edit_at, system_restore_at,
    system_show_at, system_show_at_with_mcp, without_terminal_controls,
};
use super::*;

#[cfg(test)]
mod exit_tests {
    use super::*;
    use rhema_core::ProviderError;
    use rhema_host_agent::HostAgentError;

    #[test]
    fn a_provider_failure_is_exit_code_2() {
        let err = anyhow::Error::new(HostAgentError::Agent(rhema_core::AgentError::Provider(
            ProviderError::Api {
                message: "anthropic api error (HTTP 429): rate_limit_error".into(),
                status: Some(429),
            },
        )));
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn a_wrapped_provider_failure_still_reads_as_2() {
        // The chain, not the top error, decides: the failure surfaces
        // through whatever context wrapped it on the way up.
        let err = anyhow::Error::new(HostAgentError::Agent(rhema_core::AgentError::Provider(
            ProviderError::Timeout,
        )))
        .context("the turn failed");
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn any_other_failure_is_exit_code_1() {
        assert_eq!(exit_code(&anyhow::anyhow!("no prompt")), 1);
        let missing = anyhow::Error::new(HostAgentError::Agent(
            rhema_core::AgentError::MissingProvider,
        ));
        assert_eq!(
            exit_code(&missing),
            1,
            "a driver wiring error is a rhema limit"
        );
    }

    #[test]
    fn the_failure_checkpoint_lives_under_the_workspace_rhema() {
        let path = failed_checkpoint_path(std::path::Path::new("/tmp/ws"));
        let text = path.to_string_lossy();
        assert!(text.starts_with("/tmp/ws/.rhema/runs/failed-"), "{text}");
        assert!(text.ends_with(".json"), "{text}");
    }
}

#[cfg(test)]
mod system_tests {
    use super::*;

    #[test]
    fn edit_seeds_the_default_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");

        system_edit_at(&path, Some("true")).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rhema_host_agent::DEFAULT_SYSTEM_MD
        );
    }

    #[test]
    fn edit_keeps_an_existing_file() {
        // Seeding is for the missing file only; an existing SYSTEM.md,
        // even an empty one, is the user's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");
        std::fs::write(&path, "mine").unwrap();

        system_edit_at(&path, Some("true")).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");
    }

    #[test]
    fn edit_fails_when_the_editor_exits_non_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");
        assert!(system_edit_at(&path, Some("false")).is_err());
    }

    #[test]
    fn restore_overwrites_with_the_default_and_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("SYSTEM.md");

        system_restore_at(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rhema_host_agent::DEFAULT_SYSTEM_MD
        );
    }

    #[test]
    fn show_renders_the_base_over_the_workspace_skills_and_tools() {
        let config = tempfile::tempdir().unwrap();
        let base = config.path().join("SYSTEM.md");
        std::fs::write(
            &base,
            "ws {{ workspace }}{% for skill in skills %} skill {{ skill.name }}{% endfor %}{% for tool in tools %} tool {{ tool.name }}{% endfor %}",
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let folder = workspace.path().join(".rhema").join("skills").join("local");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\ndescription: local trigger\n---\nbody",
        )
        .unwrap();

        let prompt = system_show_at(Some(&base), workspace.path(), None, None).unwrap();

        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        assert!(
            prompt.starts_with(&format!("ws {}", canonical.display())),
            "{prompt}"
        );
        assert!(prompt.contains("skill local"), "{prompt}");
        assert!(prompt.contains("tool bash"), "{prompt}");
        // The section renders only where the template places it: this
        // test's own base never references it, so it stays empty, the
        // built-in template's preview below carries it instead.
        assert!(!prompt.contains("## Environment"), "{prompt}");
    }

    #[test]
    fn show_renders_the_mcp_lines_the_session_would_disclose() {
        // A template with the mcp block, the built-in section's own
        // shape; the lines are injected, not read from the machine's
        // mcp.yaml.
        let base = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            base.path(),
            "{% for n in mcp %}- {{ n.name }} (server: {{ n.server }}): {{ n.description }}\n{% endfor %}",
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mcp = vec![rhema_config::mcp::McpServerLine {
            name: "blender".to_owned(),
            server: "blender".to_owned(),
            description: "Blender scene operations".to_owned(),
        }];
        let prompt =
            system_show_at_with_mcp(Some(base.path()), workspace.path(), None, mcp, None).unwrap();
        assert!(
            prompt.contains("- blender (server: blender): Blender scene operations"),
            "{prompt}"
        );
    }

    /// The search section's two gates: a local session, and the wire's
    /// own gate open. A preview that skipped the first would promise a
    /// `search` a sandboxed session lacks; one that skipped the second
    /// would hide the function a keychain key wires. The keyless half
    /// of the gate is the registry's own test, where the environment is
    /// injected rather than read.
    #[test]
    fn the_search_section_follows_the_local_rule_and_the_wire_gate() {
        use rhema_config::secrets::{SecretStore, VolatileSecretStore};
        use rhema_config::{Profile, WebSearchInstance};
        let store = VolatileSecretStore::new();
        store.set("ws-1", "chain-key").unwrap();
        let mut profile = Profile::default();
        profile.websearch.insert(
            "brave-main".into(),
            WebSearchInstance {
                provider: "brave".into(),
                api_key: None,
                secret_ref: Some("ws-1".into()),
            },
        );
        profile.websearch_order = vec!["brave-main".into()];
        // A key that lives only in the store still wires in the
        // session, so the section renders, and only for a local one.
        assert!(search_section(None, Some(&profile), Some(&store)));
        assert!(!search_section(Some("local"), Some(&profile), Some(&store)));
    }

    /// The decision section's two gates, the search test's shape: a
    /// local session, and the decision wire's own gate open. A key
    /// that lives only in the store still wires, so the preview must
    /// render the section for it too, and for no sandbox kind.
    #[test]
    fn the_decision_section_follows_the_local_rule_and_the_wire_gate() {
        use rhema_config::secrets::{SecretStore, VolatileSecretStore};
        use rhema_config::{DecisionInstance, DecisionSettings, Profile};
        use std::collections::BTreeMap;
        let store = VolatileSecretStore::new();
        store.set("dec-1", "chain-key").unwrap();
        let profile = Profile {
            decision: Some(DecisionSettings {
                instances: BTreeMap::from([(
                    "jev-main".to_owned(),
                    DecisionInstance {
                        provider: "jev".into(),
                        api_key: None,
                        secret_ref: Some("dec-1".into()),
                        model: None,
                        account_id: None,
                        base_url: None,
                    },
                )]),
                ..DecisionSettings::default()
            }),
            ..Profile::default()
        };
        assert!(decision_section(None, Some(&profile), Some(&store)));
        assert!(!decision_section(
            Some("docker"),
            Some(&profile),
            Some(&store)
        ));
        // No instance and no store: nothing in the profile names a
        // provider, so no section, whatever the store holds.
        assert!(!decision_section(
            None,
            Some(&Profile::default()),
            Some(&store)
        ));
    }

    /// `system show`'s preview gates its sections on questions of its
    /// own; the assembly wires the functions through
    /// `wire_for_session`. Two code paths deciding one thing can
    /// drift, and `decision` did: setup wired the macro, the preview
    /// stayed silent. This test pins each preview gate to the wire
    /// the assembly runs, over every profile shape: for the same
    /// profile and keychain, the section renders exactly when the
    /// session would wire, both directions, so a gate that changes on
    /// one side alone fails here.
    #[test]
    fn the_preview_gates_agree_with_the_wires_for_every_profile_shape() {
        use rhema_config::secrets::{SecretStore, VolatileSecretStore};
        use rhema_config::{DecisionInstance, DecisionSettings, Profile, WebSearchInstance};
        use std::collections::BTreeMap;

        let store = VolatileSecretStore::new();
        store.set("sync-key", "chain-key").unwrap();

        fn decision(instance: DecisionInstance) -> Option<DecisionSettings> {
            Some(DecisionSettings {
                instances: BTreeMap::from([("one".to_owned(), instance)]),
                ..DecisionSettings::default()
            })
        }

        let shapes: Vec<(&str, Profile)> = vec![
            ("no providers", Profile::default()),
            (
                "a decision instance with a literal key",
                Profile {
                    decision: decision(DecisionInstance {
                        provider: "openrouter".into(),
                        api_key: Some("literal".into()),
                        ..DecisionInstance::default()
                    }),
                    ..Profile::default()
                },
            ),
            (
                "a decision instance keyed by the store",
                Profile {
                    decision: decision(DecisionInstance {
                        provider: "jev".into(),
                        api_key: None,
                        secret_ref: Some("sync-key".into()),
                        ..DecisionInstance::default()
                    }),
                    ..Profile::default()
                },
            ),
            (
                "a search instance keyed by the store",
                Profile {
                    websearch: BTreeMap::from([(
                        "brave".to_owned(),
                        WebSearchInstance {
                            provider: "brave".into(),
                            api_key: None,
                            secret_ref: Some("sync-key".into()),
                        },
                    )]),
                    websearch_order: vec!["brave".into()],
                    ..Profile::default()
                },
            ),
            (
                "both",
                Profile {
                    decision: decision(DecisionInstance {
                        provider: "jev".into(),
                        api_key: None,
                        secret_ref: Some("sync-key".into()),
                        ..DecisionInstance::default()
                    }),
                    websearch: BTreeMap::from([(
                        "brave".to_owned(),
                        WebSearchInstance {
                            provider: "brave".into(),
                            api_key: None,
                            secret_ref: Some("sync-key".into()),
                        },
                    )]),
                    websearch_order: vec!["brave".into()],
                    ..Profile::default()
                },
            ),
        ];

        for (name, profile) in &shapes {
            for (n, section) in [(1usize, "decision"), (2, "websearch")] {
                // The local session each preview section assumes, and
                // the wire's own answer for the same inputs.
                let wired = match section {
                    "decision" => crate::decision_wire::wire_for_session(
                        &format!("sync-{name}-{n}"),
                        profile,
                        Some(&store),
                        None,
                        None,
                    )
                    .is_some(),
                    _ => crate::search_wire::wire_for_session(
                        &format!("sync-{name}-{n}"),
                        profile,
                        Some(&store),
                        None,
                        None,
                    )
                    .is_some(),
                };
                let previewed = match section {
                    "decision" => decision_section(None, Some(profile), Some(&store)),
                    _ => search_section(None, Some(profile), Some(&store)),
                };
                assert_eq!(
                    previewed, wired,
                    "{name}: the preview's {section} gate disagrees with the wire's"
                );
            }
        }
        // The sandbox half of each gate: no sandbox kind wires a local
        // section, the search test's own assertion, decision's now.
        assert!(!decision_section(Some("ssh"), None, None));
        assert!(!search_section(Some("ssh"), None, None));
    }

    #[test]
    fn show_falls_back_to_the_built_in_default_without_a_file() {
        let workspace = tempfile::tempdir().unwrap();

        let prompt = system_show_at(None, workspace.path(), None, None).unwrap();

        assert!(prompt.contains("bash"), "{prompt}");
        assert!(!prompt.contains("{{"), "the default is rendered: {prompt}");
        assert!(prompt.contains("## Environment"), "{prompt}");
    }

    #[test]
    fn markdown_ansi_styles_and_resets_per_span() {
        let theme = crate::palette::Palette::Rhema.theme(
            crate::palette::ThemeKind::Dark,
            crate::palette::PaletteInputs {
                m3_source: None,
                custom: None,
            },
        );
        let styled = markdown_ansi("# Title\n\nplain text", &theme, 80);
        // Style codes open, the reset closes, and no style bleeds past
        // its span into the next line.
        assert!(styled.contains('\u{1b}'), "styled at all: {styled:?}");
        assert!(styled.contains("\u{1b}[0m"), "a reset per span: {styled:?}");
        assert!(styled.ends_with('\n'));
        // Plain text alone carries no styling of its own.
        let plain = markdown_ansi("just words", &theme, 80);
        assert!(!plain.contains('\u{1b}'), "{plain:?}");
    }

    #[test]
    fn terminal_prompt_text_drops_escape_sequences_but_keeps_layout() {
        let raw = "before \u{1b}]0;title\u{7} after\tend\nnext";
        assert_eq!(
            without_terminal_controls(raw),
            "before ]0;title after\tend\nnext"
        );
    }

    #[test]
    fn the_sections_gate_routes_kinds_to_host_or_notice() {
        // The local case probes the host, either honest answer; the
        // user section rides the machine's own USER.md, so its presence
        // is not asserted.
        let (host, _, missing) =
            prompt_sections(std::env::temp_dir().as_path(), "/ws", None, "m", "h");
        let host = host.unwrap();
        assert!(host.contains("## Environment"));
        assert!(host.contains("- Model: m"));
        // A local session names what it misses; the sandboxed one below
        // must not.
        assert!(missing.iter().all(|tool| !tool.is_empty()));
        let (notice, _, missing) = prompt_sections(
            std::env::temp_dir().as_path(),
            "/ws",
            Some("docker"),
            "m",
            "h",
        );
        let notice = notice.unwrap();
        assert!(notice.contains("docker sandbox"));
        assert!(notice.contains("- Workspace: /ws"));
        assert!(missing.is_empty(), "a sandbox claims no host absences");
    }
}

/// A scroll renumbers slots: the piece that inherits a slot may be a
/// different sprite, and the placement the old sprite held must go,
/// a placement is the pair of the image id and the slot, so a slot
/// match alone would strand the old pair on screen as a ghost corner
/// of a bubble that scrolled away.
#[test]
fn a_slot_that_changes_sprite_deletes_the_old_pair() {
    use crate::ui::BubblePiece;
    let piece = |slot: u16, sprite: u32| BubblePiece {
        sprite,
        col: 0,
        row: 0,
        cols: 1,
        rows: 1,
        slot,
    };
    // Last frame held four corners under slots 1..=4.
    let placed = vec![
        piece(1, 1000),
        piece(2, 1003),
        piece(3, 1002),
        piece(4, 1001),
    ];
    // Growth only, a bubble scrolled in, takes nothing away.
    let grown = vec![
        piece(1, 1000),
        piece(2, 1003),
        piece(3, 1002),
        piece(4, 1001),
        piece(5, crate::term_image::Edge::Fill.sprite_id()),
    ];
    assert!(super::stale_placements(&placed, &grown).is_empty());
    // A scrolled-out bubble pulls the list in: slot 2 now holds a
    // different sprite, and slots 3 and 4 are gone. All three old
    // pairs must delete, slot 2's because the sprite changed, the
    // other two because nothing holds them anymore.
    let pulled_in = vec![piece(1, 1000), piece(2, 1001)];
    let stale = super::stale_placements(&placed, &pulled_in);
    assert_eq!(stale.len(), 3, "{stale:?}");
    assert!(
        stale.contains(&piece(2, 1003)),
        "the flipped slot's old pair"
    );
    assert!(stale.contains(&piece(3, 1002)));
    assert!(stale.contains(&piece(4, 1001)));
}
