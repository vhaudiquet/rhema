//! The system-prompt template: one markdown file, rendered with
//! placeholders.
//!
//! Templates are prose, not programs: the renderer substitutes a fixed
//! set of placeholders and leaves every other `{...}` as written. A
//! template naming no skills placeholder at all gets the default table
//! appended, so prompts written before `{skills}` keep working.

use std::path::Path;

/// The built-in system prompt, kept as data next to the code and read
/// in whole — the full prompt, visible and editable in one place.
/// Placeholders: see [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// Renders a SYSTEM.md template: `{workspace}` becomes the workspace
/// path, `{skills}` the skills table
/// ([`prompt_section`](crate::skills::prompt_section) — empty when no
/// skill was found, so the model never sees machinery for an empty
/// set). A template without `{skills}` gets the table appended, so a
/// prompt written before the placeholder keeps working. Any other
/// `{...}` text stays as written: templates are prose, not programs.
pub fn render_system(template: &str, workspace: &Path, skills: &[crate::skills::Skill]) -> String {
    let section = crate::skills::prompt_section(skills);
    let rendered = template
        .replace("{workspace}", &workspace.display().to_string())
        .replace("{skills}", &section);
    let rendered = if template.contains("{skills}") {
        rendered
    } else {
        rendered + &section
    };
    format!("{}\n", rendered.trim_end())
}

/// The built-in prompt for a workspace with no skills:
/// [`DEFAULT_SYSTEM_MD`] through [`render_system`] with an empty table.
pub fn default_system(workspace: &Path) -> String {
    render_system(DEFAULT_SYSTEM_MD, workspace, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> crate::skills::Skill {
        crate::skills::Skill {
            name: name.into(),
            description: description.into(),
            path: format!("/cfg/skills/{name}/SKILL.md").into(),
        }
    }

    #[test]
    fn render_fills_the_workspace_and_leaves_other_braces_alone() {
        let out = render_system("work in {workspace}, keep {mystery}", Path::new("/ws"), &[]);
        assert_eq!(out, "work in /ws, keep {mystery}\n");
    }

    #[test]
    fn render_puts_the_skills_table_where_the_placeholder_sits() {
        let out = render_system(
            "a\n{skills}\nb {workspace}",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(
            out,
            "a\n\nSkills (name / trigger / file):\ns / d / /cfg/skills/s/SKILL.md\n\nb /ws\n"
        );
    }

    #[test]
    fn render_without_the_placeholder_appends_the_table() {
        let out = render_system(
            "base in {workspace}\n",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(
            out,
            "base in /ws\n\nSkills (name / trigger / file):\ns / d / /cfg/skills/s/SKILL.md\n"
        );
    }

    #[test]
    fn render_with_no_skills_leaves_no_machinery_behind() {
        let out = render_system("a\n{skills}\nb", Path::new("/ws"), &[]);
        assert_eq!(out, "a\n\nb\n");
        assert!(!out.contains("Skills"), "{out}");
    }

    #[test]
    fn the_default_prompt_ends_with_one_newline_and_no_placeholder() {
        let out = default_system(Path::new("/ws"));
        assert!(
            out.starts_with("You are a coding agent working in the workspace /ws."),
            "{out}"
        );
        assert!(!out.contains('{'), "{out}");
        assert!(out.ends_with("workspace root.\n"), "{out}");
    }
}
