//! Wire: the frontend's one call — turn config into a live bridge, a
//! provisioned env map, and prompt lines, all filtered by the stored
//! off switch before anything spawns.
//!
//! This module is the spec's §10 consumer seam. It exists behind the
//! `rhema-coding-agent` dependency (the only place that dep is
//! allowed): the shell_env map it produces merges into
//! `CodingAgentConfig::shell_env` and the lines into the prompt's
//! `mcp` section. A sandboxed frontend wires nothing — it calls
//! nothing here, so nothing advertises what cannot run.
//!
//! The bridge rides a dedicated thread with its own runtime, so
//! [`wire`] is synchronous and safe to call from frontend code that
//! never touches an async context — and the manager's MCP work never
//! borrows the frontend's runtime, whose lifetime is the UI's, not
//! the servers'. The manager's spawned children are stateful by
//! design: they outlive any single call, and rewiring (a profile
//! switch) leaves the previous session's servers idling rather than
//! killing state a newer session might have wanted anyway.
//!
//! Teardown is drop-based and ordered by the ownership graph: dropping
//! [`Wiring`] removes the init file, stops the bridge, and the bridge
//! thread unwinds with the aborted accept task; the manager drops
//! with the wiring and its clients kill their stdio children on drop.
//! HTTP servers hold no child to clean.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rhema_agent_tools::prompt::McpServerLine;
use rhema_config::mcp::McpConfig;

use crate::manager::McpManager;
use crate::provision::{self, Provision, VAR_INHERITED};

/// What wiring produced, held together so teardown is one drop.
pub struct Wiring {
    /// The env map: merge into `CodingAgentConfig::shell_env`.
    pub shell_env: BTreeMap<String, String>,
    /// The prompt's `mcp` lines, post-filter, config order.
    pub lines: Vec<McpServerLine>,
    /// Skipped entries' notices: reserved or illegal function names,
    /// surfaced by provision.
    pub notices: Vec<String>,
    /// The manager the bridge serves; a frontend rarely touches it,
    /// but tests assert against it.
    pub manager: std::sync::Arc<McpManager>,
    socket_path: PathBuf,
    bridge: Option<crate::bridge::BridgeHandle>,
    _bridge_thread: Option<std::thread::JoinHandle<()>>,
    _provision: Provision,
}

impl Wiring {
    /// Stops the bridge accept loop and removes the socket file;
    /// teardown otherwise rides Drop.
    pub fn shutdown_bridge(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        // The thread unwinds once the aborted accept task surfaces;
        // never join-block a session teardown on it.
    }

    /// The socket path the CLI's env points at.
    pub fn socket(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for Wiring {
    fn drop(&mut self) {
        self.shutdown_bridge();
    }
}

/// The token's randomness class: cheap insurance beside the kernel's
/// socket-mode check, not the boundary itself (the socket file is
/// 0600). Time nanos and the pid, folded by FxHash-style mixing, give
/// per-session uniqueness without a crypto dependency.
fn session_token() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = u128::from(std::process::id());
    let mut state = now ^ (pid << 64) ^ 0x9E37_79B9_7F4A_7C15;
    for _ in 0..4 {
        state = state.wrapping_mul(0xFF51_AFD7_ED55_8CCD) ^ (state >> 33);
    }
    format!("{state:032x}")
}

/// Wires MCP into an agent session: filter by `disabled`, bind the
/// bridge in the session temp dir, provision the shell env and the
/// prompt lines. Synchronous: the bridge runs on a dedicated thread
/// (see the module docs), so frontend code needs no async context.
///
/// `session_tmp` is the session's temp dir (the agent's `media/`
/// rides under it); `agent_id` names the per-agent provision dir;
/// `bash_env` is the pre-existing `BASH_ENV`, read by the caller
/// before it touches the env map.
///
/// # Errors
///
/// Filesystem failures from provisioning or socket binding. Config
/// parsing never fails here: [`McpConfig::load`] is forgiving of the
/// file and strict of the entry, returning notices instead.
pub fn wire(
    config: &McpConfig,
    disabled: &BTreeSet<String>,
    session_tmp: &Path,
    agent_id: &str,
    bash_env: Option<std::ffi::OsString>,
) -> std::io::Result<Wiring> {
    wire_with_bin(
        config,
        disabled,
        session_tmp,
        agent_id,
        bash_env,
        &std::env::current_exe()?,
    )
}

/// [`wire`] over an explicit exec target — the process-level test
/// seam (see `provision_with_bin`). `#[doc(hidden)]` because nothing
/// in production has a reason to call it.
///
/// # Errors
///
/// As [`wire`].
#[doc(hidden)]
pub fn wire_with_bin(
    config: &McpConfig,
    disabled: &BTreeSet<String>,
    session_tmp: &Path,
    agent_id: &str,
    bash_env: Option<std::ffi::OsString>,
    bin: &Path,
) -> std::io::Result<Wiring> {
    // Filter before anything spawns: a disabled server gets no
    // manager slot, no stub, no prompt line.
    let filtered = McpConfig {
        servers: config
            .servers
            .iter()
            .filter(|server| !disabled.contains(&server.name))
            .cloned()
            .collect(),
        notices: Vec::new(),
    };

    let socket_path = session_tmp.join("mcp.sock");
    let socket = socket_path.clone();
    let token = session_token();
    let manager = std::sync::Arc::new(McpManager::new(&filtered));
    // The bridge's runtime lives on its own thread for the wiring's
    // lifetime: bind first, then keep the runtime parked forever. The
    // shutdown aborts the accept task, which ends block_on, which
    // ends the thread.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread_manager = manager.clone();
    let thread_socket = socket.clone();
    let thread_token = token.clone();
    let thread = std::thread::Builder::new()
        .name("mcp-bridge".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let _ = ready_tx.send(Err(std::io::Error::other(
                        "the bridge runtime failed to build",
                    )));
                    return;
                }
            };
            let result: std::io::Result<()> = runtime.block_on(async {
                let bridge =
                    crate::bridge::serve(thread_manager, thread_socket, thread_token).await?;
                let _ = ready_tx.send(Ok(bridge));
                // Park the runtime: the accept task and every handler
                // the bridge spawns run here from now on.
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            });
            if let Err(err) = result {
                let _ = ready_tx.send(Err(err));
            }
        })?;
    let bridge = ready_rx
        .recv()
        .map_err(|_| std::io::Error::other("the bridge thread died before binding"))??;
    let provisioned = provision::provision_with_bin(
        &filtered,
        bridge.socket_path(),
        &token,
        session_tmp,
        bash_env.clone(),
        agent_id,
        bin,
    )?;

    let skipped = provisioned.skipped.clone();
    let mut shell_env = provisioned.env.clone();
    // The inherited BASH_ENV must survive into the map even when
    // provision had nothing to inherit: the init file's guard reads
    // it, and the executor's env layering may carry its own — keep
    // the caller's value authoritative.
    if let Some(inherited) = bash_env {
        shell_env.insert(
            VAR_INHERITED.to_owned(),
            inherited.to_string_lossy().into_owned(),
        );
    }

    let lines = filtered
        .servers
        .iter()
        .map(|server| McpServerLine {
            name: server.command_name.clone(),
            server: server.name.clone(),
            description: server.description.clone(),
        })
        .collect();

    Ok(Wiring {
        shell_env,
        socket_path,
        lines,
        notices: skipped,
        manager,
        bridge: Some(bridge),
        _bridge_thread: Some(thread),
        _provision: provisioned,
    })
}

#[cfg(test)]
mod tests;
