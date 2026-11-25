//! Wire: the frontend's one call, turn config into a live bridge, a
//! provisioned env map, and prompt lines, all filtered by the stored
//! off switch before anything spawns.
//!
//! This module is the spec's §10 consumer seam. It exists behind the
//! `rhema-host-agent` dependency (the only place that dep is
//! allowed): the shell_env map it produces merges into
//! `HostAgentConfig::shell_env` and the lines into the prompt's
//! `mcp` section. A sandboxed frontend wires nothing, it calls
//! nothing here, so nothing advertises what cannot run.
//!
//! The bridge rides a dedicated thread with its own runtime, so
//! [`wire`] is synchronous and safe to call from frontend code that
//! never touches an async context, and the manager's MCP work never
//! borrows the frontend's runtime, whose lifetime is the UI's, not
//! the servers'. The manager's spawned children are stateful by
//! design: they outlive any single call, and rewiring (a profile
//! switch) leaves the previous session's servers idling rather than
//! killing state a newer session might have wanted anyway.
//!
//! Teardown is drop-based and ordered by the ownership graph: dropping
//! [`Wiring`] removes the init file, stops the bridge, and the stop
//! token ends the thread's park, which ends the runtime and the
//! thread; the manager drops with the wiring and its clients kill
//! their stdio children on drop.
//! HTTP servers hold no child to clean.

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rhema_config::mcp::{McpConfig, McpServerLine};

use crate::manager::McpManager;
use crate::provision::{self, Provision};

/// What wiring produced, held together so teardown is one drop.
pub struct Wiring {
    /// The env map: merge into `HostAgentConfig::shell_env`.
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
    /// The one permit that ends the bridge thread's park: the thread
    /// holds a clone and waits on it, teardown fires it. Frontend
    /// rewires drop a wiring each, so the token is what keeps a
    /// long-lived process from leaking one parked thread per rewire.
    stop: std::sync::Arc<tokio::sync::Notify>,
    _bridge_thread: Option<std::thread::JoinHandle<()>>,
    _provision: Option<Provision>,
}

impl Wiring {
    /// Stops the bridge accept loop and removes the socket file;
    /// teardown otherwise rides Drop.
    pub fn shutdown_bridge(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        // `notify_one` books a permit the park may consume whenever
        // it reaches it; never join-block a session teardown waiting
        // for it.
        self.stop.notify_one();
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
    wire_inner(
        config,
        disabled,
        session_tmp,
        agent_id,
        bash_env,
        &std::env::current_exe()?,
        false,
    )
}

/// [`wire`] over an explicit exec target, the process-level test
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
    wire_inner(config, disabled, session_tmp, agent_id, bash_env, bin, true)
}

/// The body both entries share. `write_init` is the test seam: the
/// production entry places one kit init for every surface and leaves
/// the per-agent dir without one.
fn wire_inner(
    config: &McpConfig,
    disabled: &BTreeSet<String>,
    session_tmp: &Path,
    agent_id: &str,
    bash_env: Option<std::ffi::OsString>,
    bin: &Path,
    write_init: bool,
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

    // No unix socket, no bridge: wire to nothing (empty env, no
    // lines, one notice) rather than fail the session. The frontend
    // drops an empty wiring, so the session runs without the servers.
    #[cfg(not(unix))]
    {
        let _ = (disabled, agent_id, bash_env, bin, write_init);
        Ok(Wiring {
            shell_env: BTreeMap::new(),
            socket_path: session_tmp.join("mcp.sock"),
            lines: Vec::new(),
            notices: vec![
                "mcp: the bridge binds a unix-domain socket; servers are not wired on this platform"
                    .to_owned(),
            ],
            manager: std::sync::Arc::new(McpManager::new(&filtered)),
            bridge: None,
            stop: std::sync::Arc::new(tokio::sync::Notify::new()),
            _bridge_thread: None,
            _provision: None,
        })
    }

    #[cfg(unix)]
    wire_with_bin_unix(&filtered, session_tmp, agent_id, bash_env, bin, write_init)
}

/// The unix wiring body: bind the bridge in the session temp dir,
/// provision the init file and env map. Split from [`wire_with_bin`]
/// so the non-unix refusal needs no unreachable tail.
fn wire_with_bin_unix(
    filtered: &McpConfig,
    session_tmp: &Path,
    agent_id: &str,
    bash_env: Option<std::ffi::OsString>,
    bin: &Path,
    write_init: bool,
) -> std::io::Result<Wiring> {
    let socket_path = session_tmp.join("mcp.sock");
    let socket = socket_path.clone();
    let token = session_token();
    let manager = std::sync::Arc::new(McpManager::new(filtered));
    // The bridge's runtime lives on its own thread for the wiring's
    // lifetime: bind first, then park on the stop token. Teardown
    // aborts the accept task and fires the token, which ends
    // block_on, which drops the runtime and ends the thread.
    let stop = std::sync::Arc::new(tokio::sync::Notify::new());
    let thread_stop = stop.clone();
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
                // Park the runtime on the stop token: the accept task
                // and every handler the bridge spawns run here from
                // now on, and the teardown's permit ends the park.
                thread_stop.notified().await;
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
    let provisioned = provision::provision_inner(
        filtered,
        bridge.socket_path(),
        &token,
        session_tmp,
        bash_env.clone(),
        agent_id,
        bin,
        write_init,
    )?;

    let skipped = provisioned.skipped.clone();
    let shell_env = provisioned.env.clone();

    let lines = filtered
        .servers
        .iter()
        .map(rhema_config::mcp::McpServer::prompt_line)
        .collect();

    Ok(Wiring {
        shell_env,
        socket_path,
        lines,
        notices: skipped,
        manager,
        bridge: Some(bridge),
        stop,
        _bridge_thread: Some(thread),
        _provision: Some(provisioned),
    })
}

#[cfg(test)]
mod tests;
