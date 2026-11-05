//! The claw's vision wiring: one provision for the daemon process, its
//! env merged into every coding-agent session the claw runs — chat
//! turns and `coding_agent` jobs alike. There is no config gate: the
//! `see` macro is a shell affordance, not a server, so it wires
//! whenever the daemon's agents run local shells. `apply` must run
//! before [`crate::mcp::apply`]: the MCP init file bakes this one in
//! as its literal predecessor, so `BASH_ENV` has to name the MCP file
//! last.

#![forbid(unsafe_code)]

use std::sync::{Arc, OnceLock};

use rhema_coding_agent::CodingAgentConfig;

static WIRED: OnceLock<Option<Arc<rhema_agent_tools::vision::Wiring>>> = OnceLock::new();

/// The daemon's wiring, established at first use and kept for the
/// process's life. `None` only when provisioning failed, cached like
/// every answer this static gives.
pub(crate) fn wired() -> Option<&'static Arc<rhema_agent_tools::vision::Wiring>> {
    WIRED
        .get_or_init(|| {
            rhema_agent_tools::vision::wire(std::env::var_os("BASH_ENV")).map(Arc::clone)
        })
        .as_ref()
}

/// The init file a later provisioner chains through — `None` when the
/// vision wiring itself failed, in which case the chain falls back to
/// the process's own `BASH_ENV`.
pub(crate) fn init_path() -> Option<std::ffi::OsString> {
    wired().map(|wiring| std::ffi::OsString::from(wiring.init_path.display().to_string()))
}

/// Layers the wiring onto one agent config: the shell env map (the
/// init file, the binary, the staging directory) and the prompt's
/// vision section. A frontend without wiring changes nothing.
pub(crate) fn apply(config: &mut CodingAgentConfig) {
    let Some(wired) = wired() else {
        return;
    };
    config.shell_env.extend(wired.shell_env.clone());
    config.vision = Some(wired.section.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first `apply` is indistinguishable from a second: the map's
    /// values are constants, so extending twice is the same map.
    #[test]
    fn apply_twice_is_apply_once() {
        let mut config = CodingAgentConfig::new("m", std::path::Path::new("/tmp"));
        apply(&mut config);
        let once = config.shell_env.clone();
        apply(&mut config);
        assert_eq!(config.shell_env, once);
    }
}
