//! The runner's tests: exit codes, the system actions, and the
//! prompt-section gate.

use super::headless::failed_checkpoint_path;
use super::system::{
    markdown_ansi, system_edit_at, system_restore_at, system_show_at, system_show_at_with_mcp,
};
use super::*;

#[cfg(test)]
mod exit_tests {
    use super::*;
    use rhema_coding_agent::CodingAgentError;
    use rhema_core::ProviderError;

    #[test]
    fn a_provider_failure_is_exit_code_2() {
        let err = anyhow::Error::new(CodingAgentError::Agent(rhema_core::AgentError::Provider(
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
        let err = anyhow::Error::new(CodingAgentError::Agent(rhema_core::AgentError::Provider(
            ProviderError::Timeout,
        )))
        .context("the turn failed");
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn any_other_failure_is_exit_code_1() {
        assert_eq!(exit_code(&anyhow::anyhow!("no prompt")), 1);
        let maxed = anyhow::Error::new(CodingAgentError::Agent(
            rhema_core::AgentError::MaxIterations(8),
        ));
        assert_eq!(exit_code(&maxed), 1, "a step budget is a rhema limit");
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
            rhema_coding_agent::DEFAULT_SYSTEM_MD
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
            rhema_coding_agent::DEFAULT_SYSTEM_MD
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

        let prompt = system_show_at(Some(&base), workspace.path(), None).unwrap();

        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        assert!(
            prompt.starts_with(&format!("ws {}", canonical.display())),
            "{prompt}"
        );
        assert!(prompt.contains("skill local"), "{prompt}");
        assert!(prompt.contains("tool bash"), "{prompt}");
        // The section renders only where the template places it: this
        // test's own base never references it, so it stays empty — the
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
        let mcp = vec![rhema_agent_tools::prompt::McpServerLine {
            name: "blender".to_owned(),
            server: "blender".to_owned(),
            description: "Blender scene operations".to_owned(),
        }];
        let prompt =
            system_show_at_with_mcp(Some(base.path()), workspace.path(), None, mcp).unwrap();
        assert!(
            prompt.contains("- blender (server: blender): Blender scene operations"),
            "{prompt}"
        );
    }

    #[test]
    fn show_falls_back_to_the_built_in_default_without_a_file() {
        let workspace = tempfile::tempdir().unwrap();

        let prompt = system_show_at(None, workspace.path(), None).unwrap();

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
    fn the_sections_gate_routes_kinds_to_host_or_notice() {
        // The local case probes the host — either honest answer; the
        // user section rides the machine's own USER.md, so its presence
        // is not asserted.
        let (host, _) = prompt_sections(std::env::temp_dir().as_path(), "/ws", None, "m", "h");
        let host = host.unwrap();
        assert!(host.contains("## Environment"));
        assert!(host.contains("- Model: m"));
        let (notice, _) = prompt_sections(
            std::env::temp_dir().as_path(),
            "/ws",
            Some("docker"),
            "m",
            "h",
        );
        let notice = notice.unwrap();
        assert!(notice.contains("docker sandbox"));
        assert!(notice.contains("- Workspace: /ws"));
    }
}
