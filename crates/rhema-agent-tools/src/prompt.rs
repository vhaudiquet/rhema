//! The system-prompt template renderer: one markdown template, Jinja.
//!
//! The template language is Jinja, which prompt templates across the
//! LLM ecosystem already speak. [`render_system`] is the standard
//! shape — `{{ workspace }}`, the discovered `skills`, the advertised
//! `tools`; [`render_template`] is the same engine over a caller's own
//! variables, so an agent with other per-dispatch values (the claw's
//! SOUL/USER sections, say) renders with the one syntax and the one
//! whitespace rules. Rendering is fallible on purpose: a template error
//! surfaces loudly at agent construction instead of silently rendering
//! half the prompt, while a misspelled variable renders empty
//! (minijinja's lenient undefined).

use std::path::Path;

use minijinja::{Environment, Value};
use rhema_core::ToolSpec;

use crate::skills::Skill;

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
    skills: &[Skill],
    tools: &[ToolSpec],
) -> Result<String, minijinja::Error> {
    let context = serde_json::json!({
        "workspace": workspace.display().to_string(),
        "skills": skills,
        "tools": tools,
    });
    render_template(template, &context)
}

/// Renders `template` over `context` — the same engine and whitespace
/// rules as [`render_system`], with the caller's own variables. A
/// value's own text is inserted verbatim, never re-parsed, so a
/// section's braces survive; the template name in errors is `system`.
pub fn render_template(
    template: &str,
    context: &serde_json::Value,
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
        .render(Value::from(minijinja::value::Serde(context)))?
        .trim_end()
        .to_owned()
        + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
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

    #[test]
    fn render_template_takes_the_callers_own_variables() {
        let out = render_template(
            "hi {{ who }}{% if tag %} ({{ tag }}){% endif %}",
            &json!({ "who": "claw", "tag": "" }),
        )
        .unwrap();
        assert_eq!(out, "hi claw\n");
    }

    #[test]
    fn a_values_own_text_is_inserted_verbatim_never_reparsed() {
        let out = render_template(
            "{{ section }}",
            &json!({ "section": "- braces stay: {{ keep }} and {soul}\n" }),
        )
        .unwrap();
        assert_eq!(out, "- braces stay: {{ keep }} and {soul}\n");
    }
}
