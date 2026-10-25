//! The skills rhema ships with: seeded into the user's global skills
//! directory on start, never overwritten once they are there.
//!
//! A bundled skill is an ordinary skill — a folder with a `SKILL.md`
//! whose frontmatter carries the trigger — so the whole existing
//! pipeline serves it: discovery reads it like any global skill, the
//! model `cat`s the body by path, and a user folder of the same name
//! shadows it. [`seed`] materializes the enabled ones into the global
//! directory, creating nothing that is already there: a file the user
//! edited keeps its edits, and a skill the user switched off (the
//! config's `skills_disabled` set) is not re-created until they switch
//! it back on.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

/// One skill shipped in the binary: its folder name, its default on/off
/// state, its `SKILL.md` body verbatim, and — where the skill's license
/// asks for a notice to travel with copies — that notice verbatim.
#[derive(Debug)]
pub struct BundledSkill {
    /// The folder name the skill seeds under — also the name the
    /// `skills_disabled` set switches it off by.
    pub name: &'static str,
    /// Whether a fresh install carries the skill on.
    pub default_on: bool,
    /// The `SKILL.md` body, embedded at compile time.
    pub body: &'static str,
    /// The `NOTICE` written beside the `SKILL.md`, when the skill's
    /// license asks for one (adapted code keeps its attribution).
    pub notice: Option<&'static str>,
}

/// Every bundled skill, in display order. Both default on: the commit
/// skill triggers on its own ("when needing to commit"), and unslop's
/// trigger is "Always apply".
pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "commit-message",
        default_on: true,
        body: include_str!("../skills/commit-message/SKILL.md"),
        notice: None,
    },
    BundledSkill {
        name: "unslop",
        default_on: true,
        body: include_str!("../skills/unslop/SKILL.md"),
        notice: Some(include_str!("../skills/unslop/NOTICE")),
    },
];

/// Seeds the bundled skills into `global`: every skill not disabled and
/// not already on disk gets its folder and `SKILL.md`. An existing file
/// is left untouched — user edits win, which is why there is no
/// un-seed. Errors name the folder they failed on; a partial seed is a
/// fine outcome (the next start finishes it).
pub fn seed(global: &Path, disabled: &BTreeSet<String>) -> io::Result<()> {
    for skill in BUNDLED_SKILLS {
        if disabled.contains(skill.name) {
            continue;
        }
        let folder = global.join(skill.name);
        let file = folder.join("SKILL.md");
        if file.exists() {
            continue;
        }
        std::fs::create_dir_all(&folder)
            .map_err(|err| annotate(err, skill.name, "create the folder"))?;
        std::fs::write(&file, skill.body)
            .map_err(|err| annotate(err, skill.name, "write SKILL.md"))?;
        if let Some(notice) = skill.notice {
            std::fs::write(folder.join("NOTICE"), notice)
                .map_err(|err| annotate(err, skill.name, "write NOTICE"))?;
        }
    }
    Ok(())
}

/// Seeds one bundled skill by name, whatever the disabled set says — the
/// re-enable path, where the user just switched a skill back on and the
/// file it should have is missing. `false` when no bundled skill carries
/// the name or nothing needed seeding.
pub fn seed_one(global: &Path, name: &str) -> io::Result<bool> {
    let Some(skill) = BUNDLED_SKILLS.iter().find(|skill| skill.name == name) else {
        return Ok(false);
    };
    let file = global.join(name).join("SKILL.md");
    if file.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(global.join(name))
        .map_err(|err| annotate(err, name, "create the folder"))?;
    std::fs::write(&file, skill.body).map_err(|err| annotate(err, name, "write SKILL.md"))?;
    if let Some(notice) = skill.notice {
        std::fs::write(global.join(name).join("NOTICE"), notice)
            .map_err(|err| annotate(err, name, "write NOTICE"))?;
    }
    Ok(true)
}

/// Whether a bundled skill carries this name.
pub fn is_bundled(name: &str) -> bool {
    BUNDLED_SKILLS.iter().any(|skill| skill.name == name)
}

/// A failure annotated with the skill and the step that failed, so a
/// read-only config directory says what it refused to do.
fn annotate(err: io::Error, name: &str, step: &str) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("cannot {step} for the bundled skill {name:?}: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_writes_enabled_skills_and_skips_existing_and_disabled() {
        let dir = tempfile::tempdir().unwrap();
        // A user edit in place before any seeding.
        let edited = dir.path().join("commit-message").join("SKILL.md");
        std::fs::create_dir_all(edited.parent().unwrap()).unwrap();
        std::fs::write(&edited, "mine now").unwrap();
        let disabled = BTreeSet::from(["unslop".to_owned()]);

        seed(dir.path(), &disabled).unwrap();

        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "mine now");
        assert!(!dir.path().join("unslop").exists(), "disabled stays away");
        // Both bundled skills are default-on; an empty disabled set
        // seeds the lot.
        seed(dir.path(), &BTreeSet::new()).unwrap();
        assert!(dir.path().join("unslop").join("SKILL.md").is_file());
        // Idempotent: a second run touches nothing and fails nothing.
        seed(dir.path(), &BTreeSet::new()).unwrap();
    }

    #[test]
    fn seed_one_materializes_only_the_named_skill() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seed_one(dir.path(), "unslop").unwrap());
        assert!(!seed_one(dir.path(), "unslop").unwrap(), "already there");
        assert!(!seed_one(dir.path(), "not-bundled").unwrap());
        assert!(dir.path().join("unslop").join("SKILL.md").is_file());
        assert!(!dir.path().join("commit-message").exists());
    }

    #[test]
    fn every_bundled_body_carries_a_trigger_frontmatter() {
        // The same contract discovery enforces: a skill without a
        // description is not a skill, and a bundled body that lost its
        // frontmatter would seed a folder discovery then ignores.
        for skill in BUNDLED_SKILLS {
            let rest = skill
                .body
                .strip_prefix("---\n")
                .unwrap_or_else(|| panic!("{} carries no frontmatter block", skill.name));
            let (block, _) = rest
                .split_once("\n---")
                .unwrap_or_else(|| panic!("{} frontmatter never closes", skill.name));
            assert!(
                block.lines().any(|line| {
                    line.starts_with("description:") && line.trim().len() > "description:".len() + 1
                }),
                "{} carries no description trigger",
                skill.name
            );
        }
    }
}
