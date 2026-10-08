//! The system-prompt template: one markdown file, rendered with
//! placeholders.
//!
//! Templates are prose, not programs: the renderer substitutes
//! `{workspace}`, repeats the one bounded block, and leaves every other
//! `{...}` as written. A template without the block simply presents no
//! skills.

use std::path::Path;

/// The built-in system prompt, kept as data next to the code and read
/// in whole — the full prompt, visible and editable in one place.
/// Placeholders: see [`render_system`].
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// Renders a SYSTEM.md template: `{workspace}` becomes the workspace
/// path, and a `{#skills}…{/skills}` block renders its body once per
/// skill with `{skill.name}`, `{skill.description}`, and
/// `{skill.path}` filled — vanishing entirely when there are none, so
/// the model never sees machinery for an empty set. An unclosed block
/// stays as written. Any other `{...}` text stays as written, and a
/// template without the block presents no skills at all: templates are
/// prose, not programs.
pub fn render_system(template: &str, workspace: &Path, skills: &[crate::skills::Skill]) -> String {
    let rendered = template.replace("{workspace}", &workspace.display().to_string());
    let rendered = render_blocks(&rendered, skills);
    format!("{}\n", rendered.trim_end())
}

/// The built-in prompt for a workspace with no skills:
/// [`DEFAULT_SYSTEM_MD`] through [`render_system`] with an empty table.
pub fn default_system(workspace: &Path) -> String {
    render_system(DEFAULT_SYSTEM_MD, workspace, &[])
}

/// Renders every `{#skills}…{/skills}` block: the body once per skill,
/// the per-skill fields filled; the whole block gone when there are no
/// skills. An unclosed block is prose and stays as written.
fn render_blocks(template: &str, skills: &[crate::skills::Skill]) -> String {
    let Some((before, rest)) = template.split_once("{#skills}") else {
        return template.to_owned();
    };
    let Some((body, after)) = rest.split_once("{/skills}") else {
        return template.to_owned();
    };
    let mut out = String::from(before);
    for skill in skills {
        out.push_str(
            &body
                .replace("{skill.name}", &skill.name)
                .replace("{skill.description}", &skill.description)
                .replace("{skill.path}", &skill.path.display().to_string()),
        );
    }
    out.push_str(&render_blocks(after, skills));
    out
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
    fn the_block_renders_its_body_once_per_skill() {
        let out = render_system(
            "{#skills}- {skill.name}: {skill.description} — cat {skill.path}\n{/skills}",
            Path::new("/ws"),
            &[skill("a", "da"), skill("b", "db")],
        );
        assert_eq!(
            out,
            "- a: da — cat /cfg/skills/a/SKILL.md\n- b: db — cat /cfg/skills/b/SKILL.md\n"
        );
    }

    #[test]
    fn an_empty_block_vanishes_whole() {
        let out = render_system("top\n{#skills}x{/skills}\nend", Path::new("/ws"), &[]);
        assert_eq!(out, "top\n\nend\n");
        assert!(!out.contains('x'), "{out}");
    }

    #[test]
    fn an_unclosed_block_stays_as_written() {
        let out = render_system(
            "a {#skills} never closed",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(out, "a {#skills} never closed\n");
    }

    #[test]
    fn unknown_field_names_inside_the_block_stay_as_written() {
        let out = render_system(
            "{#skills}{skill.nope} {skill.name}{/skills}",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(out, "{skill.nope} s\n");
    }

    #[test]
    fn a_template_without_the_block_presents_no_skills() {
        let out = render_system(
            "base in {workspace}\n",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(out, "base in /ws\n");
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

    #[test]
    fn the_default_prompt_lists_skills_through_its_block() {
        let out = render_system(DEFAULT_SYSTEM_MD, Path::new("/ws"), &[skill("s", "d")]);
        assert!(
            out.contains("s / d / /cfg/skills/s/SKILL.md"),
            "the per-skill line renders: {out}"
        );
        assert!(
            !out.contains("Skills ("),
            "the block carries no heading of its own: {out}"
        );
    }
}
