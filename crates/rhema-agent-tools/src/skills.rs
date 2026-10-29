//! Skills: markdown instruction files the model loads on demand.
//!
//! A skill is a folder holding a `SKILL.md` whose YAML frontmatter
//! carries a `description` — the trigger. Only name, trigger, and file
//! reach the system prompt, as the [`crate::prompt`] template's
//! `skills` list; the model reads a skill's body itself with the
//! bash tool (`cat`) when a trigger applies, so skills need no tool of
//! their own, no approval, and no machinery beyond discovery. Skills
//! shadow by folder name across three tiers. The workspace's
//! `.rhema/skills` wins over rhema's global directory, which wins over
//! the cross-agent `~/.agents/skills`. A rhema skill of the same name
//! hides the `.agents` one, never the reverse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// The folder's name.
    pub name: String,
    /// The frontmatter description — the trigger shown to the model.
    pub description: String,
    /// The frontmatter license, when the skill declares one — a short
    /// SPDX name or a name-and-copyright line the UIs show under the
    /// description. Absent on skills authored into their repository.
    pub license: Option<String>,
    /// Absolute path of the `SKILL.md` the model reads.
    pub path: PathBuf,
}

/// The `SKILL.md` frontmatter this crate reads. Everything else in the
/// block is ignored.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    description: String,
    license: Option<String>,
}

/// Discovers skills from the lowest shadowing tier up: the cross-agent
/// `agents` directory, rhema's own `global` directory, then the
/// workspace's `.rhema/skills`. A name found twice appears once, the
/// higher tier shadowing the lower by folder name, so a skill in
/// either rhema directory hides the `.agents` copy of the same name.
/// A folder is a skill only when its `SKILL.md` parses with a
/// `description`; the trigger is the whole point, and an undescribed
/// skill has nothing to show the model.
pub fn discover(agents: Option<&Path>, global: Option<&Path>, workspace: &Path) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    if let Some(agents) = agents {
        collect_into(agents, &mut by_name);
    }
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
        let Some((description, license)) = parse_description(&raw) else {
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
                license,
                path: file,
            },
        );
    }
}

/// The `description` out of a leading `---` frontmatter block — and the
/// optional `license`; `None` for the description when there is no
/// block or no field, which makes the folder not a skill at all.
fn parse_description(raw: &str) -> Option<(String, Option<String>)> {
    let rest = raw.strip_prefix("---\n")?;
    let block = rest.split_once("\n---")?.0;
    let front: Frontmatter = serde_yaml::from_str(block).ok()?;
    let description = front.description.trim();
    (!description.is_empty()).then(|| {
        (
            description.to_owned(),
            front
                .license
                .map(|license| license.trim().to_owned())
                .filter(|license| !license.is_empty()),
        )
    })
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

        let skills = discover(None, Some(global.path()), workspace.path());
        assert_eq!(
            skills,
            vec![
                Skill {
                    name: "askill".into(),
                    description: "the a trigger".into(),
                    license: None,
                    path: workspace.path().join(".rhema/skills/askill/SKILL.md"),
                },
                Skill {
                    name: "zskill".into(),
                    description: "the z trigger".into(),
                    license: None,
                    path: global.path().join("zskill/SKILL.md"),
                },
            ]
        );
    }

    #[test]
    fn the_license_field_rides_the_frontmatter_when_present() {
        let global = tempfile::tempdir().unwrap();
        let folder = global.path().join("borrowed");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\ndescription: a trigger\nlicense: MIT (c) someone\n---\nbody",
        )
        .unwrap();

        let skills = discover(None, Some(global.path()), Path::new("/nowhere"));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].license.as_deref(), Some("MIT (c) someone"));
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

        let skills = discover(None, Some(global.path()), workspace.path());
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

        assert!(discover(None, Some(global.path()), Path::new("/nowhere")).is_empty());
    }

    #[test]
    fn the_cross_agent_agents_directory_is_discovered() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents").join("skills"),
            "shared",
            "agents trigger",
        );

        let skills = discover(
            Some(&home.path().join(".agents").join("skills")),
            None,
            workspace.path(),
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "agents trigger");
        assert!(
            skills[0]
                .path
                .starts_with(home.path().join(".agents").join("skills")),
            "the body reads from the .agents tree"
        );
    }

    #[test]
    fn a_rhema_skill_hides_the_agents_copy_of_the_same_name() {
        let home = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents").join("skills"),
            "shared",
            "agents trigger",
        );
        write_skill(global.path(), "shared", "global trigger");
        write_skill(
            &workspace.path().join(".rhema").join("skills"),
            "shared",
            "workspace trigger",
        );

        let skills = discover(
            Some(&home.path().join(".agents").join("skills")),
            Some(global.path()),
            workspace.path(),
        );
        assert_eq!(skills.len(), 1, "one name, one skill");
        assert_eq!(skills[0].description, "workspace trigger");

        // Without the workspace copy, rhema's global still wins — the
        // .agents folder only serves names nothing above it claims.
        let skills = discover(
            Some(&home.path().join(".agents").join("skills")),
            Some(global.path()),
            Path::new("/nowhere"),
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "global trigger");
    }

    #[test]
    fn agents_skills_sit_alongside_rhema_ones_when_the_names_differ() {
        let home = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents").join("skills"),
            "askill",
            "agents trigger",
        );
        write_skill(global.path(), "zskill", "global trigger");

        let skills = discover(
            Some(&home.path().join(".agents").join("skills")),
            Some(global.path()),
            workspace.path(),
        );
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["askill", "zskill"],
            "sorted by name, both present"
        );
    }

    #[test]
    fn missing_directories_discover_to_nothing() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            discover(None, Some(Path::new("/nowhere")), workspace.path()).is_empty(),
            "no global dir is normal, not an error"
        );
    }
}
