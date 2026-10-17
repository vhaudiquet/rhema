//! Subagent definitions: markdown files the parent delegates to.
//!
//! A definition is a folder holding a `SUBAGENT.md` whose YAML
//! frontmatter carries a `description` (the trigger the parent sees),
//! an optional `model`, and an optional `max_steps`; the body is the
//! subagent's system prompt. Only well-formed definitions exist:
//! a folder whose file lacks a non-empty description or a non-empty
//! body is not a subagent, not an error — a subagent without a trigger
//! has nothing to show the parent, and one without instructions has no
//! prompt to run on. Project definitions shadow global ones by folder
//! name.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// The file a definition folder holds.
pub const FILE: &str = "SUBAGENT.md";

/// The step budget when a definition sets no `max_steps`.
pub const DEFAULT_MAX_STEPS: u32 = 256;

/// One discovered subagent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentDef {
    /// The folder's name.
    pub name: String,
    /// The frontmatter description — the trigger shown to the parent in
    /// the `subagent` tool's catalog.
    pub description: String,
    /// The model identifier the subagent's requests carry; `None` uses
    /// the parent's model.
    pub model: Option<String>,
    /// The subagent's step budget; `None` uses [`DEFAULT_MAX_STEPS`].
    pub max_steps: Option<u32>,
    /// The body: the subagent's system prompt.
    pub body: String,
}

impl SubagentDef {
    /// The step budget this definition runs with.
    pub fn max_steps(&self) -> u32 {
        self.max_steps.unwrap_or(DEFAULT_MAX_STEPS)
    }
}

/// The `SUBAGENT.md` frontmatter this crate reads; everything else in
/// the block is ignored.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    description: String,
    model: Option<String>,
    max_steps: Option<u32>,
}

/// Discovers subagent definitions: the global directory first, then the
/// workspace's `.rhema/subagents`, project shadowing global by folder
/// name. A folder is a definition only when its `SUBAGENT.md` parses
/// with a non-empty description and a non-empty body.
pub fn discover(global: Option<&Path>, workspace: &Path) -> Vec<SubagentDef> {
    let mut by_name: BTreeMap<String, SubagentDef> = BTreeMap::new();
    if let Some(global) = global {
        collect_into(global, &mut by_name);
    }
    collect_into(&workspace.join(".rhema").join("subagents"), &mut by_name);
    by_name.into_values().collect()
}

/// Reads every definition directly under `dir` into the map, keyed by
/// folder name.
fn collect_into(dir: &Path, by_name: &mut BTreeMap<String, SubagentDef>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file = path.join(FILE);
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some(def) = parse(&raw) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_name.insert(
            name.clone(),
            SubagentDef {
                name,
                description: def.description,
                model: def.model,
                max_steps: def.max_steps,
                body: def.body,
            },
        );
    }
}

/// Parses one `SUBAGENT.md` into its fields; `None` when the frontmatter
/// or the body is missing or empty, or when `max_steps` is set to zero.
fn parse(raw: &str) -> Option<SubagentDef> {
    let rest = raw.strip_prefix("---\n")?;
    let (block, body) = rest.split_once("\n---")?;
    let front: Frontmatter = serde_yaml::from_str(block).ok()?;
    let description = front.description.trim();
    if description.is_empty() {
        return None;
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let model = front
        .model
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty());
    let max_steps = match front.max_steps {
        // A budget of zero can never finish a turn: the definition is
        // malformed, skip it like a missing description.
        Some(0) => return None,
        steps => steps,
    };
    Some(SubagentDef {
        name: String::new(),
        description: description.to_owned(),
        model,
        max_steps,
        body: body.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_def(dir: &Path, name: &str, raw: &str) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join(FILE);
        std::fs::write(&file, raw).unwrap();
        file
    }

    fn simple(description: &str) -> String {
        format!("---\ndescription: {description}\n---\n\nDo the thing.\n")
    }

    #[test]
    fn discovery_reads_global_and_project_sorted_by_name() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_def(global.path(), "zeta", &simple("the z trigger"));
        write_def(
            &workspace.path().join(".rhema").join("subagents"),
            "alpha",
            &simple("the a trigger"),
        );

        let defs = discover(Some(global.path()), workspace.path());
        assert_eq!(
            defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(defs[0].description, "the a trigger");
        assert_eq!(defs[0].body, "Do the thing.");
        assert_eq!(defs[0].model, None);
        assert_eq!(defs[0].max_steps, None);
        assert_eq!(defs[0].max_steps(), DEFAULT_MAX_STEPS);
    }

    #[test]
    fn a_project_definition_shadows_a_global_one_by_name() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_def(global.path(), "shared", &simple("global trigger"));
        write_def(
            &workspace.path().join(".rhema").join("subagents"),
            "shared",
            &simple("project trigger"),
        );

        let defs = discover(Some(global.path()), workspace.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "project trigger");
    }

    #[test]
    fn frontmatter_options_parse() {
        let raw = "---\ndescription: t\nmodel: zai/glm\nmax_steps: 12\n---\n\nBody.\n";
        let def = parse(raw).unwrap();
        assert_eq!(def.model.as_deref(), Some("zai/glm"));
        assert_eq!(def.max_steps, Some(12));
        assert_eq!(def.max_steps(), 12);
    }

    #[test]
    fn malformed_definitions_are_not_definitions() {
        // No frontmatter block.
        let no_block = "---\nno closing\n";
        assert!(parse(no_block).is_none());
        // Frontmatter without the field.
        let undescribed = "---\nname: x\n---\n\nBody.\n";
        assert!(parse(undescribed).is_none());
        // Empty description.
        let empty = "---\ndescription: \"\"\n---\n\nBody.\n";
        assert!(parse(empty).is_none());
        // Missing body.
        let bodiless = "---\ndescription: t\n---\n\n   \n";
        assert!(parse(bodiless).is_none());
        // A zero step budget can never finish.
        let budgetless = "---\ndescription: t\nmax_steps: 0\n---\n\nBody.\n";
        assert!(parse(budgetless).is_none());
        // A blank model is no model.
        let blank_model = "---\ndescription: t\nmodel: \"  \"\n---\n\nBody.\n";
        let def = parse(blank_model).unwrap();
        assert_eq!(def.model, None);

        // And discovery skips the folders wholesale.
        let global = tempfile::tempdir().unwrap();
        write_def(global.path(), "no-block", no_block);
        write_def(global.path(), "undescribed", undescribed);
        write_def(global.path(), "bodiless", bodiless);
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
