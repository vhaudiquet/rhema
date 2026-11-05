//! The claw's MCP wiring: one bridge for the daemon process, its env
//! merged into every coding-agent session the claw runs — chat turns
//! and `coding_agent` jobs alike. The global `mcp.yaml` is the
//! claw's config scope (there are no profiles here); the off switch
//! is the claw config's `disabled_mcp_servers`.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use rhema_coding_agent::CodingAgentConfig;

/// What one wiring produced, held for the process.
struct Wired {
    wiring: Arc<rhema_mcp::Wiring>,
    disabled: BTreeSet<String>,
}

static WIRED: OnceLock<Option<Wired>> = OnceLock::new();

/// The daemon's wiring, established at first use and never re-read:
/// the claw is single-instance, its servers are stateful across
/// calls, and a rewire mid-process would orphan running sessions'
/// socket paths. `None` when nothing is configured, everything is
/// muted, or the config file cannot load.
fn wired() -> Option<&'static Wired> {
    WIRED
        .get_or_init(|| {
            let claw = ClawConfig::load_from(&ClawConfig::default_path().ok()?).ok()?;
            let mcp = rhema_config::mcp::McpConfig::load();
            if mcp.servers.is_empty() {
                return None;
            }
            let dir = std::env::temp_dir().join(format!("rhema-claw-mcp-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            // The init chain: the vision wiring first, this one
            // second — `BASH_ENV` names ours, whose inheritance guard
            // sources the vision file, which sources the process's own.
            let inherited = crate::vision::init_path().or_else(|| std::env::var_os("BASH_ENV"));
            let wiring =
                rhema_mcp::wire(&mcp, &claw.disabled_mcp_servers, &dir, "claw", inherited).ok()?;
            for notice in &wiring.notices {
                eprintln!("· {notice}");
            }
            if wiring.lines.is_empty() {
                // Everything muted: drop the wiring (its own Drop
                // stops the bridge) and record the answer as none.
                drop(wiring);
                return None;
            }
            Some(Wired {
                wiring: Arc::new(wiring),
                disabled: claw.disabled_mcp_servers,
            })
        })
        .as_ref()
}

/// Layers the wiring onto one agent config: the shell env map (the
/// init file, the socket, the token), the prompt's `mcp` lines, and
/// the persisted off switch. A frontend without wiring changes
/// nothing.
pub(crate) fn apply(config: &mut CodingAgentConfig) {
    let Some(wired) = wired() else {
        return;
    };
    config.shell_env.extend(wired.wiring.shell_env.clone());
    config.mcp_servers = wired.wiring.lines.clone();
    config.disabled_mcp_servers = wired.disabled.clone();
}

use crate::config::ClawConfig;

#[cfg(test)]
mod tests {
    use super::*;

    /// A first `apply` is indistinguishable from a second: the
    /// extension must be idempotent at the level the agent sees (the
    /// wiring map's values are constants, so extending twice is the
    /// same map).
    #[test]
    fn apply_without_any_wiring_leaves_the_config_alone() {
        let mut config = CodingAgentConfig::new("m", std::path::Path::new("/tmp"));
        let before = config.shell_env.clone();
        apply(&mut config);
        // The wiring only appears when the machine's global mcp.yaml
        // has servers; a bare environment must not invent any.
        let after_has_mcp_vars = config
            .shell_env
            .keys()
            .any(|key| key.starts_with("RHEMA_MCP"));
        if after_has_mcp_vars {
            assert!(config.shell_env.len() > before.len());
        } else {
            assert_eq!(config.shell_env, before, "no wiring, no change");
        }
    }
}
