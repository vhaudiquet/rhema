//! MCP wiring for the local frontends: one bridge, one provisioned
//! env, one set of prompt lines, per session key, held in the
//! registry its owner hands it.
//!
//! A sandboxed session wires only when the backend shares a runtime
//! root and the kit helper is installed there: the bridge binds under
//! the shared root, so the sandbox-side stub reaches the same socket
//! path. Without a shared root, or before the install, nothing is
//! advertised. Rewiring on a profile switch leaves the old
//! session's servers idling, the manager's children are stateful by
//! design, and a playwright browser outliving a profile switch is a
//! feature, not a leak.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rhema_bash::runtime::KIT_VERSION;
use rhema_config::Profile;
use rhema_config::mcp::McpConfig;

/// One session's wiring: the live bridge and provision, plus the
/// session temp dir that owns the socket's directory.
pub(crate) struct Entry {
    wiring: rhema_mcp::Wiring,
    /// Held for the directory's lifetime, the socket file and the
    /// CLI's `RHEMA_MCP_TMP` live under it.
    #[allow(dead_code)]
    session_dir: SessionDir,
}

/// The directory a session's socket and media live in. The local path
/// owns a random `TempDir`; the sandboxed path owns a directory under
/// the shared runtime root, so the socket path is the same on both
/// sides, and removes it on drop.
enum SessionDir {
    Temp(tempfile::TempDir),
    Shared(PathBuf),
}

impl SessionDir {
    fn path(&self) -> &Path {
        match self {
            Self::Temp(dir) => dir.path(),
            Self::Shared(path) => path,
        }
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        if let Self::Shared(path) = self {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// The per-process key a shared session dir is named under: the
/// frontend's key plus the pid, so two processes on one profile do
/// not bind the same socket. The sandbox sees the same name.
pub(crate) fn runtime_session_key(session_key: &str) -> String {
    let clean: String = session_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    format!("{clean}-{}", std::process::id())
}

/// Creates a session dir `0700` under the shared root.
fn shared_session_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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
        // The session dir owns the socket's directory. Local gets a
        // private, random temp dir (`tempfile` creates it `0700`
        // atomically, so another user cannot pre-create a symlink);
        // a mounted sandbox gets `<root>/mcp/<key>`, the path both
        // sides see. The bridge and the CLI share it through
        // `RHEMA_MCP_TMP`.
        let (session_dir, helper) = match sandboxed {
            Some(sandboxed) => {
                let shared = sandboxed.shared()?;
                let helper = shared.install_dir(&KIT_VERSION.to_string()).join("rhema");
                // The kit placement installs the helper in the shared
                // root; until it is there the stub has nothing to exec,
                // so the session advertises nothing.
                if !helper.is_file() {
                    return None;
                }
                let dir = shared.mcp_dir(&runtime_session_key(session_key));
                shared_session_dir(&dir).ok()?;
                (SessionDir::Shared(dir), Some(helper))
            }
            None => (
                SessionDir::Temp(
                    tempfile::Builder::new()
                        .prefix("rhema-mcp-session-")
                        .tempdir()
                        .ok()?,
                ),
                None,
            ),
        };
        let mut wiring = rhema_mcp::wire(
            &merged,
            all_disabled,
            session_dir.path(),
            session_key,
            bash_env,
        )
        .ok()?;
        if let Some(helper) = helper {
            // The host's own executable is the wrong architecture for a
            // sandbox on another machine: the stub execs the installed
            // helper, at the path both sides see.
            wiring.shell_env.insert(
                "RHEMA_MCP_BIN".to_owned(),
                helper.to_string_lossy().into_owned(),
            );
        }
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

        let tmp = second
            .shell_env()
            .get("RHEMA_MCP_TMP")
            .cloned()
            .expect("session tmp");
        assert!(
            !second.shell_env().contains_key("BASH_ENV"),
            "the kit placement names the one init file, not the wiring"
        );
        assert!(
            std::path::Path::new(&tmp).join("mcp.sock").exists(),
            "the session dir carries the bridge socket"
        );

        // The registry still caches the second entry; dropping the
        // registry is the app's exit path.
        drop(first);
        drop(second);
        assert!(std::path::Path::new(&tmp).exists());

        drop(reg);

        assert!(
            !std::path::Path::new(&tmp).exists(),
            "the session dir went with the entry"
        );
    }

    #[test]
    fn a_sandboxed_session_binds_under_the_shared_root() {
        let root = tempfile::tempdir().expect("runtime root");
        let install = root.path().join("bin").join(KIT_VERSION.to_string());
        std::fs::create_dir_all(&install).expect("install dir");
        let helper = install.join("rhema");
        std::fs::write(&helper, "#!/bin/sh\nexit 0\n").expect("helper");

        let mut profile = Profile::default();
        profile
            .mcp_servers
            .insert("counter".to_owned(), entry("a counter"));
        let sandboxed = crate::sandbox::Sandboxed {
            choice: crate::args::SandboxChoice::Docker,
            target: "box".to_owned(),
            tools: std::sync::Arc::new(rhema_host_agent::ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
            shared: Some(rhema_sandbox::kit::SharedRoot::new(root.path())),
        };
        let reg = Registry::new();
        let disabled = std::collections::BTreeSet::new();
        let entry = reg
            .wire_for_session("sandbox-key", &profile, &disabled, Some(&sandboxed), None)
            .expect("wires against the shared root");
        let tmp = entry
            .shell_env()
            .get("RHEMA_MCP_TMP")
            .cloned()
            .expect("session tmp");
        assert!(
            std::path::Path::new(&tmp).starts_with(root.path().join("mcp")),
            "the socket dir rides the shared root: {tmp}"
        );
        assert_eq!(
            entry.shell_env().get("RHEMA_MCP_BIN").map(String::as_str),
            Some(helper.to_string_lossy().as_ref()),
            "the stub execs the installed helper, not the host binary"
        );
        let socket = entry
            .shell_env()
            .get("RHEMA_MCP_BRIDGE")
            .cloned()
            .expect("bridge path");
        assert!(
            std::path::Path::new(&socket).starts_with(root.path().join("mcp")),
            "the bridge binds under the shared root: {socket}"
        );
        drop(entry);
        drop(reg);
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
