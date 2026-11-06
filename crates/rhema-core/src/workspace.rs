//! The workspace directory conventions every driver and frontend shares.
//!
//! A rhema checkout carries a marker directory, `.rhema` at its root —
//! the project's own skills, subagent definitions, and run records live
//! there, shadowing the user-global tiers rhema-config's `Home`
//! carries. The paths are a single vocabulary: whoever reads or writes
//! one goes through these functions, so the convention is written once,
//! not hunted down as string literals across five crates.

use std::path::{Path, PathBuf};

/// The workspace's marker directory (`<workspace>/.rhema`) — the root
/// the project keeps its rhema-side files under.
pub fn rhema_dir(workspace: &Path) -> PathBuf {
    workspace.join(".rhema")
}

/// The workspace's skill tier (`<workspace>/.rhema/skills`) — the top
/// of the shadowing order, over the global directories.
pub fn skills_dir(workspace: &Path) -> PathBuf {
    rhema_dir(workspace).join("skills")
}

/// The workspace's subagent tier (`<workspace>/.rhema/subagents`).
pub fn subagents_dir(workspace: &Path) -> PathBuf {
    rhema_dir(workspace).join("subagents")
}

/// Where run records land (`<workspace>/.rhema/runs`) — failed-turn
/// checkpoints and other artifacts a session leaves behind.
pub fn runs_dir(workspace: &Path) -> PathBuf {
    rhema_dir(workspace).join("runs")
}
