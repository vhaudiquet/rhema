//! The system-prompt template: one markdown file, rendered by
//! minijinja.
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

use minijinja::{Environment, Value};
use rhema_core::ToolSpec;

/// The built-in system prompt, kept as data next to the code and read
/// in whole — the full prompt, visible and editable in one place,
/// tools included. Template syntax: see [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// Renders a SYSTEM.md template: `{{ workspace }}` becomes the
/// workspace path, `skills` holds the discovered skills as `name`,
/// `description`, and `path` fields, and `tools` holds the advertised
/// tool specs as `name`, `description`, and `parameters` (the JSON
/// schema, renderable with `tojson`) — all there for
/// `{% for %}`/`{% if %}`. Undefined variables render empty; syntax and
/// filter errors return with template positions.
pub fn render_system(
    template: &str,
    workspace: &Path,
    skills: &[crate::skills::Skill],
    tools: &[ToolSpec],
) -> Result<String, minijinja::Error> {
    let mut env = Environment::new();
    // Block tags may sit on their own lines in a markdown template
    // without blank-line runoff.
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .trim_blocks(true)
            .lstrip_blocks(true)
            .build()?,
    );
    env.add_template("system", template)?;
    Ok(env
        .get_template("system")?
        .render(minijinja::context! {
            workspace => workspace.display().to_string(),
            skills => Value::from(minijinja::value::Serde(skills)),
            tools => Value::from(minijinja::value::Serde(tools)),
        })?
        .trim_end()
        .to_owned()
        + "\n")
}

/// The built-in prompt for a workspace with no skills: [`DEFAULT_SYSTEM_MD`]
/// over the built-in tools — the prompt an agent with no user override and
/// an empty skill list stamps into every request. Infallible: the built-in
/// template is validated by tests.
pub fn default_system(workspace: &Path) -> String {
    render_system(
        DEFAULT_SYSTEM_MD,
        workspace,
        &[],
        &crate::tools::CodingTools::builtin_specs(),
    )
    .expect("the built-in template is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn skill(name: &str, description: &str) -> crate::skills::Skill {
        crate::skills::Skill {
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

    #[test]
    fn render_fills_the_workspace_and_leaves_other_braces_alone() {
        let out = render_system(
            "work in {{ workspace }}, keep {mystery}",
            Path::new("/ws"),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(out, "work in /ws, keep {mystery}\n");
    }

    #[test]
    fn a_for_loop_renders_its_body_once_per_skill() {
        let out = render_system(
            "{% for skill in skills %}- {{ skill.name }}: {{ skill.description }} — cat {{ skill.path }}\n{% endfor %}",
            Path::new("/ws"),
            &[skill("a", "da"), skill("b", "db")],
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "- a: da — cat /cfg/skills/a/SKILL.md\n- b: db — cat /cfg/skills/b/SKILL.md\n"
        );
    }

    #[test]
    fn a_for_loop_renders_its_body_once_per_tool_with_the_schema() {
        let out = render_system(
            "{% for tool in tools %}- {{ tool.name }}: {{ tool.description }} — {{ tool.parameters | tojson }}\n{% endfor %}",
            Path::new("/ws"),
            &[],
            &[tool("t", "dt")],
        )
        .unwrap();
        assert_eq!(
            out,
            "- t: dt — {\"properties\": {\"command\": {\"type\": \"string\"}}, \"required\": [\"command\"], \"type\": \"object\"}\n"
        );
    }

    #[test]
    fn an_if_around_the_skills_section_holds_a_vanishing_heading() {
        let template = "{% if skills %}Skills (name / trigger / file):\n{% for skill in skills %}{{ skill.name }} / {{ skill.description }} / {{ skill.path }}\n{% endfor %}{% endif %}";
        let with = render_system(template, Path::new("/ws"), &[skill("s", "d")], &[]).unwrap();
        assert_eq!(
            with,
            "Skills (name / trigger / file):\ns / d / /cfg/skills/s/SKILL.md\n"
        );
        let without = render_system(template, Path::new("/ws"), &[], &[]).unwrap();
        assert_eq!(without, "\n");
    }

    #[test]
    fn a_misspelled_variable_renders_empty_not_an_error() {
        let out = render_system(
            "{% for skill in skills %}{{ skill.nope }}{{ skill.name }}{% endfor %}",
            Path::new("/ws"),
            &[skill("s", "d")],
            &[],
        )
        .unwrap();
        assert_eq!(out, "s\n");
    }

    #[test]
    fn a_template_error_surfaces_instead_of_rendering_prose() {
        let err = render_system("{% if skills %} never closed", Path::new("/ws"), &[], &[])
            .expect_err("an unclosed block is a loud error, not prose");
        assert!(err.to_string().contains("system"), "{err}");
    }

    #[test]
    fn a_template_without_the_block_presents_no_skills() {
        let out = render_system(
            "base in {{ workspace }}\n",
            Path::new("/ws"),
            &[skill("s", "d")],
            &[],
        )
        .unwrap();
        assert_eq!(out, "base in /ws\n");
    }

    // The three default-prompt tests below pin the structural contract —
    // markers resolve, fields reach the model — never the wording: the
    // prompt prose is the user's to edit without breaking tests.

    #[test]
    fn the_default_prompt_resolves_every_marker_and_ends_with_one_newline() {
        let out = default_system(Path::new("/ws"));
        assert!(!out.contains("{{") && !out.contains("{%"), "{out}");
        assert!(out.contains("/ws"), "the workspace named: {out}");
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
