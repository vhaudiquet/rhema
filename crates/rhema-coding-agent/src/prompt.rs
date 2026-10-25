//! The coding agent's system-prompt template: one markdown file,
//! rendered by the toolkit's Jinja renderer
//! ([`rhema_agent_tools::render_system`]).
//!
//! The template language is Jinja, which prompt templates across the
//! LLM ecosystem already speak: `{{ workspace }}` names the session's
//! workspace, `skills` is the discovered skill list, `tools` is the
//! advertised tool spec list, and the template decides itself where —
//! and whether — those lists appear. Rendering is fallible on purpose:
//! a template error surfaces loudly at agent construction instead of
//! silently rendering half the prompt, while a misspelled variable
//! renders empty (minijinja's lenient undefined).

use std::path::Path;

use rhema_agent_tools::{ShellTools, render_system};

/// The built-in system prompt, kept as data next to the code and read
/// in whole — the full prompt, visible and editable in one place,
/// tools included. Template syntax: see [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// The built-in prompt for a workspace with no skills: [`DEFAULT_SYSTEM_MD`]
/// over the built-in tools — the prompt an agent with no user override and
/// an empty skill list stamps into every request. Infallible: the built-in
/// template is validated by tests.
pub fn default_system(workspace: &Path) -> String {
    render_system(
        DEFAULT_SYSTEM_MD,
        workspace,
        &[],
        &ShellTools::builtin_specs(),
    )
    .expect("the built-in template is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ToolSpec;
    use serde_json::json;

    fn skill(name: &str, description: &str) -> rhema_agent_tools::Skill {
        rhema_agent_tools::Skill {
            name: name.into(),
            description: description.into(),
            path: format!("/cfg/skills/{name}/SKILL.md").into(),
        }
    }

    fn tool(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            description,
            json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        )
    }

    // The default-prompt tests below pin the structural contract —
    // markers resolve, fields reach the model — never the wording: the
    // prompt prose is the user's to edit without breaking tests.

    #[test]
    fn the_default_prompt_resolves_every_marker_and_ends_with_one_newline() {
        let out = default_system(Path::new("/ws"));
        assert!(!out.contains("{{") && !out.contains("{%"), "{out}");
        // The bare prompt names no workspace: the fact rides the
        // environment section (the report's CWD, the notice's
        // Workspace), which a section-less render carries empty.
        assert!(!out.contains("/ws"), "{out}");
        assert!(out.ends_with('\n') && !out.ends_with("\n\n"), "{out}");
    }

    #[test]
    fn the_default_prompt_presents_each_skill_field_to_the_model() {
        let out = render_system(
            DEFAULT_SYSTEM_MD,
            Path::new("/ws"),
            &[skill("my-skill", "its trigger text")],
            &[],
        )
        .unwrap();
        assert!(out.contains("my-skill"), "{out}");
        assert!(out.contains("its trigger text"), "{out}");
        assert!(out.contains("/cfg/skills/my-skill/SKILL.md"), "{out}");
    }

    #[test]
    fn the_default_prompt_presents_each_tool_field_to_the_model() {
        let out = render_system(
            DEFAULT_SYSTEM_MD,
            Path::new("/ws"),
            &[],
            &[tool("my-tool", "its purpose")],
        )
        .unwrap();
        assert!(out.contains("my-tool"), "{out}");
        assert!(out.contains("its purpose"), "{out}");
        assert!(
            out.contains(r#""required": ["command"]"#),
            "the JSON schema rendered: {out}"
        );
    }

    #[test]
    fn the_default_prompt_names_the_built_in_bash_tool() {
        let out = default_system(Path::new("/ws"));
        assert!(out.contains("bash"), "{out}");
        // The same description the API's tools array carries.
        assert!(out.contains("Run a shell command (`bash -c`)"), "{out}");
    }
}
