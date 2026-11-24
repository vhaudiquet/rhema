//! The host agent's system-prompt template: one markdown file,
//! rendered by the prompt crate's Jinja renderer
//! ([`rhema_prompt::render_system`]).
//!
//! The template language is Jinja, which prompt templates across the
//! LLM ecosystem already speak: `{{ workspace }}` names the session's
//! workspace, `skills` is the discovered skill list, `tools` is the
//! advertised tool spec list, and the template decides itself where,
//! and whether, those lists appear. Rendering is fallible on purpose:
//! a template error surfaces loudly at agent construction instead of
//! silently rendering half the prompt, while a misspelled variable
//! renders empty (minijinja's lenient undefined).

use std::path::Path;

use rhema_bash::ShellTools;
use rhema_prompt::{Prompt, render_system};

/// The built-in system prompt, kept as data next to the code and read
/// in whole, the full prompt, visible and editable in one place,
/// tools included. Template syntax: see [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// The built-in prompt for a workspace with no skills: [`DEFAULT_SYSTEM_MD`]
/// over the built-in tools, the prompt an agent with no user override and
/// an empty skill list stamps into every request. Infallible: the built-in
/// template is validated by tests.
pub fn default_system(workspace: &Path) -> String {
    render_system(
        DEFAULT_SYSTEM_MD,
        &Prompt::new(workspace, &[], &ShellTools::builtin_specs()),
    )
    .expect("the built-in template is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ToolSpec;
    use serde_json::json;

    fn skill(name: &str, description: &str) -> rhema_prompt::Skill {
        rhema_prompt::Skill {
            name: name.into(),
            description: description.into(),
            license: None,
            path: format!("/cfg/skills/{name}/SKILL.md").into(),
            tier: rhema_prompt::SkillTier::Global,
            body: "the rules".into(),
        }
    }

    /// The built-in template over a skills and tools list, no sections.
    fn render(
        skills: &[rhema_prompt::Skill],
        tools: &[ToolSpec],
    ) -> Result<String, rhema_prompt::TemplateError> {
        render_system(
            DEFAULT_SYSTEM_MD,
            &Prompt::new(Path::new("/ws"), skills, tools),
        )
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

    // The default-prompt tests below pin the structural contract,
    // markers resolve, fields reach the model, never the wording: the
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
    fn the_websearch_section_renders_only_when_wired() {
        // The template's conditional: a frontend that wired nothing
        // advertises nothing.
        let bare = default_system(Path::new("/ws"));
        assert!(!bare.contains("Web search"), "{bare}");
        let wired = render(&[], &ShellTools::builtin_specs());
        // The section rides the same interpolation the vision and
        // jobs sections do; here we only pin the conditional's two
        // branches against the template's own variable.
        assert!(wired.is_ok());
        let template = DEFAULT_SYSTEM_MD;
        assert!(template.contains("{% if websearch %}"), "{template}");
        assert!(template.contains("{{ websearch }}"), "{template}");
        // The decision section rides the same conditional.
        assert!(!bare.contains("Decision models"), "{bare}");
        assert!(template.contains("{% if decision %}"), "{template}");
        assert!(template.contains("{{ decision }}"), "{template}");
    }

    #[test]
    fn the_default_prompt_presents_each_skill_field_to_the_model() {
        let out = render(&[skill("my-skill", "its trigger text")], &[]).unwrap();
        assert!(out.contains("my-skill"), "{out}");
        assert!(out.contains("its trigger text"), "{out}");
        assert!(out.contains("/cfg/skills/my-skill/SKILL.md"), "{out}");
    }

    #[test]
    fn the_default_prompt_presents_each_tool_field_to_the_model() {
        let out = render(&[], &[tool("my-tool", "its purpose")]).unwrap();
        assert!(out.contains("my-tool"), "{out}");
        assert!(out.contains("its purpose"), "{out}");
        assert!(
            out.contains(r#""required":["command"]"#),
            "the JSON schema rendered: {out}"
        );
    }

    /// A tool row is `name | arguments` with the description indented
    /// underneath: a multi-line description stays inside its bullet, so
    /// the schema can never end up grouped with the last catalog line.
    #[test]
    fn a_multi_line_description_stays_inside_its_bullet() {
        let out = render(
            &[],
            &[tool(
                "my-tool",
                "its purpose.\n\nSubagents, by name:\n- one: does a thing",
            )],
        )
        .unwrap();
        assert!(out.contains("- my-tool | {"), "{out}");
        assert!(out.contains("\n  its purpose."), "{out}");
        assert!(out.contains("\n  Subagents, by name:"), "{out}");
        assert!(out.contains("\n  - one: does a thing"), "{out}");
        assert!(!out.contains("\nSubagents, by name:"), "{out}");
    }

    #[test]
    fn the_default_prompt_names_the_built_in_bash_tool() {
        let out = default_system(Path::new("/ws"));
        assert!(out.contains("bash"), "{out}");
        // The same description the API's tools array carries.
        assert!(out.contains("Run a bash script (`bash -c`)"), "{out}");
    }
}
