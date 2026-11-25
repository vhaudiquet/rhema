//! MCP wiring for the local frontends: one bridge, one provisioned
//! env, one set of prompt lines, per session key, held in the
//! registry its owner hands it.
//!
//! The gate is the spec's §10 rule: a sandboxed session wires
//! nothing, so nothing advertises what cannot run (`RunRequest`
//! carries no environment, a provisioned map could not reach the
//! executor anyway). Rewiring on a profile switch leaves the old
//! session's servers idling, the manager's children are stateful by
//! design, and a playwright browser outliving a profile switch is a
//! feature, not a leak.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use rhema_config::Profile;
use rhema_config::mcp::McpConfig;

/// One session's wiring: the live bridge and provision, plus the
/// session temp dir that owns the socket's directory.
pub(crate) struct Entry {
    wiring: rhema_mcp::Wiring,
    /// Held for the directory's lifetime, the socket file and the
    /// CLI's `RHEMA_MCP_TMP` live under it. A `TempDir` creates it
    /// `0700`, gives it a random name, and removes it on drop, so
    /// nothing else can pre-create or guess the path.
    #[allow(dead_code)]
    session_dir: tempfile::TempDir,
}

impl Entry {
    /// The env map to merge into the agent's `shell_env`.
    pub(crate) fn shell_env(&self) -> &BTreeMap<String, String> {
        &self.wiring.shell_env
    }

    /// The prompt's `mcp` lines, post-filter.
    pub(crate) fn lines(&self) -> &[rhema_config::mcp::McpServerLine] {
        &self.wiring.lines
    }
}

/// The registry: one entry per session key. A key is the profile's
/// identity, switching profiles rewires; staying on one reuses the
/// bridge and its already-spawned servers. The owner holds it for as
/// long as its sessions run, so dropping the owner drops every entry
/// and removes their dirs, no static and no exit hook.
pub(crate) struct Registry {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
}

impl Registry {
    /// An empty registry.
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// The merged config for one session: the global file plus the
/// profile's own entries, profile entries winning by name (the more
/// specific scope). Notices from both folds ride along, they are
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

impl Registry {
    /// Drops one session's wiring: the bridge stops, its spawned servers
    /// die, the next `wire_for_session` on this key builds fresh. The
    /// settings screen calls this when a server is ADDED or REMOVED,
    /// the tool surface changed, so the cached wiring is stale by
    /// definition. (The mute toggle deliberately does NOT rewire: its
    /// prompt-side filter hides the row without killing every other
    /// server's state, which is the whole point of the registry cache.)
    pub(crate) fn rewire(&self, session_key: &str) {
        if let Ok(mut reg) = self.entries.lock() {
            // Removing drops the Arc<Entry>; if the old agent held no
            // other reference the bridge dies here, before the
            // rebuild's wire_for_session spawns a fresh one. An
            // in-flight turn's calls fail against the dead socket and
            // the model retries, the same shape as any server death.
            reg.remove(session_key);
        }
    }

    /// The wiring for one session, reusing an existing one on the same
    /// key. `None` when nothing is configured, everything is muted, or
    /// the session is sandboxed, all three mean "advertise nothing".
    pub(crate) fn wire_for_session(
        &self,
        session_key: &str,
        profile: &Profile,
        disabled: &BTreeSet<String>,
        sandboxed: Option<&crate::sandbox::Sandboxed>,
        bash_env: Option<std::ffi::OsString>,
    ) -> Option<Arc<Entry>> {
        if sandboxed.is_some() {
            return None;
        }
        let mut reg = self.entries.lock().expect("mcp registry lock");
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
        // A private, random session dir: `tempfile` creates it `0700`
        // atomically, so another user cannot pre-create a symlink at a
        // predictable path under the system temp dir. The bridge and the
        // generated CLI share it through `RHEMA_MCP_TMP`, not through a
        // fixed name.
        let session_dir = tempfile::Builder::new()
            .prefix("rhema-mcp-session-")
            .tempdir()
            .ok()?;
        let wiring = rhema_mcp::wire(
            &merged,
            all_disabled,
            session_dir.path(),
            session_key,
            bash_env,
        )
        .ok()?;
        for notice in &wiring.notices {
            eprintln!("· {notice}");
        }
        if wiring.lines.is_empty() {
            // Everything configured is muted: no wiring to keep. Dropping
            // is the teardown, the Wiring's own Drop stops the bridge.
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
    fn rewire_replaces_the_wiring_for_its_key() {
        let reg = Registry::new();
        let mut profile = Profile::default();
        profile
            .mcp_servers
            .insert("counter".to_owned(), entry("a counter"));
        let key = "rewire-key";
        let disabled = std::collections::BTreeSet::new();
        let first = reg
            .wire_for_session(key, &profile, &disabled, None, None)
            .expect("wires");
        let token_one = first
            .shell_env()
            .get("RHEMA_MCP_TOKEN")
            .cloned()
            .expect("token");

        reg.rewire(key);

        let second = reg
            .wire_for_session(key, &profile, &disabled, None, None)
            .expect("rewires");
        let token_two = second
            .shell_env()
            .get("RHEMA_MCP_TOKEN")
            .cloned()
            .expect("token");
        assert_ne!(
            token_one, token_two,
            "the rewire built a fresh wiring, not the cache"
        );

        let init = second
            .shell_env()
            .get("BASH_ENV")
            .cloned()
            .expect("init path");
        let tmp = second
            .shell_env()
            .get("RHEMA_MCP_TMP")
            .cloned()
            .expect("session tmp");

        // What the two dirs hold while they live: the provision dir
        // is the init file alone, the session dir the bridge socket.
        let provision_dir = std::path::Path::new(&init)
            .parent()
            .expect("the init file has a dir");
        let placed: Vec<_> = std::fs::read_dir(provision_dir)
            .expect("provision dir")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            placed,
            vec![std::ffi::OsString::from("init.bash")],
            "no binary rides in the provision dir"
        );
        assert!(
            std::path::Path::new(&tmp).join("mcp.sock").exists(),
            "the session dir carries the bridge socket"
        );

        // The registry still caches the second entry; dropping the
        // registry is the app's exit path.
        drop(first);
        drop(second);
        assert!(std::path::Path::new(&init).exists());
        assert!(std::path::Path::new(&tmp).exists());

        drop(reg);

        assert!(
            !std::path::Path::new(&init).exists(),
            "the provision dir went with the entry"
        );
        assert!(
            !std::path::Path::new(&tmp).exists(),
            "the session dir went with the entry"
        );
    }

    #[test]
    fn a_profile_without_servers_wires_nothing() {
        // The gate short-circuits before any registry or filesystem
        // work: no config means no bridge thread, no provision dir.
        let profile = Profile::default();
        assert!(
            Registry::new()
                .wire_for_session(
                    "empty-key",
                    &profile,
                    &BTreeSet::new(),
                    None as Option<&crate::sandbox::Sandboxed>,
                    None,
                )
                .is_none(),
            "nothing configured wires nothing"
        );
    }
}
