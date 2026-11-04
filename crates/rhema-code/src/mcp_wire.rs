//! MCP wiring for the local frontends: one bridge, one provisioned
//! env, one set of prompt lines — per session key, held for the
//! process.
//!
//! The gate is the spec's §10 rule: a sandboxed session wires
//! nothing, so nothing advertises what cannot run (`ExecRequest`
//! carries no environment, a provisioned map could not reach the
//! executor anyway). Rewiring on a profile switch leaves the old
//! session's servers idling — the manager's children are stateful by
//! design, and a playwright browser outliving a profile switch is a
//! feature, not a leak.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rhema_config::Profile;
use rhema_config::mcp::McpConfig;

/// One session's wiring: the live bridge and provision, plus the
/// session temp dir that owns the socket's directory.
pub(crate) struct Entry {
    wiring: rhema_mcp::Wiring,
    /// Held for the directory's lifetime — the socket file and the
    /// CLI's `RHEMA_MCP_TMP` live under it; nothing reads the path
    /// again.
    #[allow(dead_code)]
    session_dir: PathBuf,
}

impl Entry {
    /// The env map to merge into the agent's `shell_env`.
    pub(crate) fn shell_env(&self) -> &BTreeMap<String, String> {
        &self.wiring.shell_env
    }

    /// The prompt's `mcp` lines, post-filter.
    pub(crate) fn lines(&self) -> &[rhema_agent_tools::prompt::McpServerLine] {
        &self.wiring.lines
    }
}

/// The registry: one entry per session key. A key is the profile's
/// identity — switching profiles rewires; staying on one reuses the
/// bridge and its already-spawned servers.
static WIRED: OnceLock<Mutex<BTreeMap<String, Arc<Entry>>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<String, Arc<Entry>>> {
    WIRED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The merged config for one session: the global file plus the
/// profile's own entries, profile entries winning by name (the more
/// specific scope). Notices from both folds ride along — they are
/// user-config diagnostics and belong in the startup log.
pub(crate) fn merged_config(profile: &Profile) -> McpConfig {
    let global = McpConfig::load();
    let own = McpConfig::from_entries(&profile.mcp_servers);
    let mut notices = global.notices.clone();
    notices.extend(own.notices.iter().cloned());
    let mut by_name: BTreeMap<String, _> = global
        .servers
        .into_iter()
        .map(|server| (server.name.clone(), server))
        .collect();
    for server in own.servers {
        by_name.insert(server.name.clone(), server);
    }
    McpConfig {
        servers: by_name.into_values().collect(),
        notices,
    }
}

/// The wiring for one session, reusing an existing one on the same
/// key. `None` when nothing is configured, everything is muted, or
/// the session is sandboxed — all three mean "advertise nothing".
pub(crate) fn wire_for_session(
    session_key: &str,
    profile: &Profile,
    disabled: &BTreeSet<String>,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
) -> Option<Arc<Entry>> {
    if sandboxed.is_some() {
        return None;
    }
    let mut reg = registry().lock().expect("mcp registry lock");
    if let Some(entry) = reg.get(session_key) {
        return Some(entry.clone());
    }
    let merged = merged_config(profile);
    if merged.servers.is_empty() {
        return None;
    }
    for notice in &merged.notices {
        eprintln!("· {notice}");
    }
    let all_disabled = disabled;
    if all_disabled.contains(&String::from("*")) {
        return None;
    }
    // The session dir owns the socket's directory; the init file
    // lives in its own 0700 provision dir inside the system temp dir,
    // not here.
    let session_dir = std::env::temp_dir().join(format!(
        "rhema-mcp-session-{}-{session_key}",
        std::process::id()
    ));
    std::fs::create_dir_all(&session_dir).ok()?;
    let inherited = std::env::var_os("BASH_ENV");
    let wiring =
        rhema_mcp::wire(&merged, all_disabled, &session_dir, session_key, inherited).ok()?;
    for notice in &wiring.notices {
        eprintln!("· {notice}");
    }
    if wiring.lines.is_empty() {
        // Everything configured is muted: no wiring to keep. Dropping
        // is the teardown — the Wiring's own Drop stops the bridge.
        drop(wiring);
        return None;
    }
    let entry = Arc::new(Entry {
        wiring,
        session_dir,
    });
    reg.insert(session_key.to_owned(), entry.clone());
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(description: &str) -> rhema_config::mcp::McpServerEntry {
        serde_yaml::from_str(&format!("description: {description}\ncommand: [true]"))
            .expect("parses")
    }

    #[test]
    fn the_merged_config_lets_the_profile_win_by_name() {
        let mut profile = Profile::default();
        profile
            .mcp_servers
            .insert("profile-only".to_owned(), entry("only here"));
        let merged = merged_config(&profile);
        assert!(
            merged.servers.iter().any(|s| s.name == "profile-only"),
            "the profile's own server rides: {:?}",
            merged
                .servers
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_profile_without_servers_wires_nothing() {
        // The gate short-circuits before any registry or filesystem
        // work: no config means no bridge thread, no provision dir.
        let profile = Profile::default();
        assert!(
            wire_for_session(
                "empty-key",
                &profile,
                &BTreeSet::new(),
                None as Option<&crate::sandbox::Sandboxed>
            )
            .is_none(),
            "nothing configured wires nothing"
        );
    }
}
