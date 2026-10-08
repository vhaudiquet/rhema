//! Skills: markdown instruction files the model loads on demand.
//!
//! A skill is a folder holding a `SKILL.md` whose YAML frontmatter
//! carries a `description` — the trigger. Only name, trigger, and file
//! reach the system prompt, through the [`crate::prompt`] template's
//! `{#skills}` block; the model reads a skill's body itself with the
//! bash tool (`cat`) when a trigger applies, so skills need no tool of
//! their own, no approval, and no machinery beyond discovery. Project
//! skills shadow global ones by folder name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The folder's name.
    pub name: String,
    /// The frontmatter description — the trigger shown to the model.
    pub description: String,
    /// Absolute path of the `SKILL.md` the model reads.
    pub path: PathBuf,
}

/// The `SKILL.md` frontmatter this crate reads. Everything else in the
/// block is ignored.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    description: String,
}

/// Discovers skills: the global directory first, then the workspace's
/// `.rhema/skills`, project shadowing global by folder name. A folder is
/// a skill only when its `SKILL.md` parses with a `description`; the
/// trigger is the whole point, and an undescribed skill has nothing to
/// show the model.
pub fn discover(global: Option<&Path>, workspace: &Path) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    if let Some(global) = global {
        collect_into(global, &mut by_name);
    }
    collect_into(&workspace.join(".rhema").join("skills"), &mut by_name);
    by_name.into_values().collect()
}

/// Reads every skill directly under `dir` into the map, keyed by folder
/// name.
fn collect_into(dir: &Path, by_name: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file = path.join("SKILL.md");
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some(description) = parse_description(&raw) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_name.insert(
            name.clone(),
            Skill {
                name,
                description,
                path: file,
            },
        );
    }
}

/// The `description` out of a leading `---` frontmatter block; `None`
/// when there is no block or no field.
fn parse_description(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("---\n")?;
    let block = rest.split_once("\n---")?.0;
    let front: Frontmatter = serde_yaml::from_str(block).ok()?;
    let description = front.description.trim();
    (!description.is_empty()).then(|| description.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, description: &str) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("SKILL.md");
        std::fs::write(
            &file,
            format!("---\ndescription: {description}\n---\n\nDo the thing.\n"),
        )
        .unwrap();
        file
    }

    #[test]
    fn discovery_reads_global_and_project_sorted_by_name() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_skill(global.path(), "zskill", "the z trigger");
        write_skill(
            &workspace.path().join(".rhema").join("skills"),
            "askill",
            "the a trigger",
        );

        let skills = discover(Some(global.path()), workspace.path());
        assert_eq!(
            skills,
            vec![
                Skill {
                    name: "askill".into(),
                    description: "the a trigger".into(),
                    path: workspace.path().join(".rhema/skills/askill/SKILL.md"),
                },
                Skill {
                    name: "zskill".into(),
                    description: "the z trigger".into(),
                    path: global.path().join("zskill/SKILL.md"),
                },
            ]
        );
    }

    #[test]
    fn a_project_skill_shadows_a_global_one_by_name() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_skill(global.path(), "shared", "global trigger");
        write_skill(
            &workspace.path().join(".rhema").join("skills"),
            "shared",
            "project trigger",
        );

        let skills = discover(Some(global.path()), workspace.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project trigger");
        assert!(skills[0].path.starts_with(workspace.path()));
    }

    #[test]
    fn folders_without_a_described_skill_md_are_not_skills() {
        let global = tempfile::tempdir().unwrap();
        // No frontmatter block.
        let empty = global.path().join("no-block");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("SKILL.md"), "just text").unwrap();
        // Frontmatter without the field.
        let undescribed = global.path().join("no-description");
        std::fs::create_dir_all(&undescribed).unwrap();
        std::fs::write(undescribed.join("SKILL.md"), "---\nname: x\n---\nbody").unwrap();
        // A bare file, not a folder.
        std::fs::write(global.path().join("stray.md"), "x").unwrap();

        assert!(discover(Some(global.path()), Path::new("/nowhere")).is_empty());
    }

    #[test]
    fn missing_directories_discover_to_nothing() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            discover(Some(Path::new("/nowhere")), workspace.path()).is_empty(),
            "no global dir is normal, not an error"
        );
    }
}
