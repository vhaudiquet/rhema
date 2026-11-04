//! The manager: config to live connections, one slot per server.
//!
//! Each configured server gets a slot guarding a [`tokio::sync::Mutex`]
//! over its connection state — the mutex *is* the single-flight
//! connect: the first caller locks, spawns, and handshakes; callers
//! arriving mid-handshake block on the same lock and find the
//! connection ready (or the death) instead of spawning a second
//! playwright browser the manager then forgets. Two concurrent calls
//! hitting a dead server's cooldown window produce at most one respawn
//! attempt for the same reason.
//!
//! Death handling follows the spec's contract: a dead server fails
//! in-band with its stderr tail, the slot cools down for
//! [`RESPAWN_COOLDOWN`], and the first call after the cooldown
//! respawns through the same lock. Calls inside the cooldown fail fast
//! with the recorded tail — a server that died has nothing new to say
//! ninety times a second.
//!
//! The catalog is fetched once per connection and cached until death
//! or an explicit refresh: `tools/list` with pagination follows
//! cursors to the end inside [`Client::tools_list`], so the manager
//! caches its result, never re-walking pages.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhema_config::mcp::{McpConfig, McpServer};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::client::{Cancel, Client, ClientError, HttpParams, StdioParams};
use crate::protocol::ToolDef;

/// How long a dead server's slot refuses respawn attempts: spec §6.
pub const RESPAWN_COOLDOWN: Duration = Duration::from_secs(5);

/// Why a manager operation failed: the server is not in config, or
/// the underlying connection errored (transport, protocol, timeout,
/// cancellation, RPC error) — the CLI maps [`ClientError`] to exit
/// codes, the manager does not decide.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// No configured server by this name.
    #[error("unknown server `{0}`")]
    UnknownServer(String),
    /// The connection layer failed; the inner error carries the tail.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The slot is cooling: the server died and the respawn window has
    /// not opened. The tail is the death's last stderr.
    #[error("server `{server}` is down; respawn cooling down ({tail})")]
    Cooling {
        /// The server's config name.
        server: String,
        /// The death's last stderr, capped.
        tail: String,
    },
}

/// One server's slot: config plus the guarded state.
struct Slot {
    config: McpServer,
    state: Mutex<SlotState>,
}

/// What a slot is currently doing.
enum SlotState {
    /// Nothing spawned yet; first use will connect.
    Disconnected,
    /// A live connection and its cached catalog.
    Connected {
        client: Arc<Client>,
        catalog: Vec<ToolDef>,
    },
    /// A death was detected at `died`; respawn attempts before
    /// `until` fail fast with `tail`.
    Cooling { until: Instant, tail: String },
}

/// The coarse state a slot reports without spawning anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SlotSummary {
    /// Configured, never used.
    #[serde(rename = "idle")]
    Idle,
    /// A live connection.
    #[serde(rename = "running")]
    Running,
    /// Dead; respawn becomes possible `retry_after_secs` from now.
    #[serde(rename = "cooldown")]
    Cooling {
        /// Seconds until the next call may respawn the server.
        retry_after_secs: u64,
    },
}

/// What `mcp servers` and the bridge's `/servers` report, one per
/// configured server. Status never spawns: an idle stdio server reads
/// `idle` because the only honest answer without a spawn is absence.
#[derive(Clone, Debug, Serialize)]
pub struct ServerStatus {
    /// The config key.
    pub name: String,
    /// The user-written description, verbatim.
    pub description: String,
    /// The generated command's name.
    pub command_name: String,
    /// The coarse state.
    pub state: SlotSummary,
    /// The last death's stderr tail, when the slot is cooling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
}

/// The configured servers and their slots.
pub struct McpManager {
    slots: BTreeMap<String, Arc<Slot>>,
    cooldown: Duration,
}

impl McpManager {
    /// Builds a manager over a validated config; duplicate names
    /// cannot occur (validation rejects them upstream), and later
    /// duplicates would win by map insertion anyway.
    pub fn new(config: &McpConfig) -> Self {
        let slots: BTreeMap<String, Arc<Slot>> = config
            .servers
            .iter()
            .map(|server| {
                (
                    server.name.clone(),
                    Arc::new(Slot {
                        config: server.clone(),
                        state: Mutex::new(SlotState::Disconnected),
                    }),
                )
            })
            .collect();
        Self {
            slots,
            cooldown: RESPAWN_COOLDOWN,
        }
    }

    /// Overrides the respawn cooldown — provisioning for tests, which
    /// cannot afford five seconds of wall clock per death.
    pub fn with_respawn_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// The configured servers' statuses, no spawns.
    pub async fn statuses(&self) -> Vec<ServerStatus> {
        let mut out = Vec::new();
        for slot in self.slots.values() {
            let state = slot.state.lock().await;
            let (summary, tail) = match &*state {
                SlotState::Disconnected => (SlotSummary::Idle, None),
                SlotState::Connected { .. } => (SlotSummary::Running, None),
                SlotState::Cooling { until, tail } => {
                    let left = until.saturating_duration_since(Instant::now());
                    (
                        SlotSummary::Cooling {
                            retry_after_secs: left.as_secs() + u64::from(left.subsec_millis() > 0),
                        },
                        Some(tail.clone()),
                    )
                }
            };
            out.push(ServerStatus {
                name: slot.config.name.clone(),
                description: slot.config.description.clone(),
                command_name: slot.config.command_name.clone(),
                state: summary,
                tail,
            });
        }
        out
    }

    /// The server's tool catalog, connecting on first use and cached
    /// until death or [`McpManager::refresh`].
    pub async fn catalog(&self, server: &str) -> Result<Vec<ToolDef>, ManagerError> {
        let handle = self.ensure_connected(server).await?;
        Ok(handle.catalog.clone())
    }

    /// Drops the cached catalog; the next [`McpManager::catalog`]
    /// re-walks `tools/list` over the live connection. An idle server
    /// stays idle: there is no cache to drop and nothing to spawn for.
    pub async fn refresh(&self, server: &str) -> Result<(), ManagerError> {
        let slot = self.slot(server)?;
        let mut state = slot.state.lock().await;
        if let SlotState::Connected { client, .. } = &*state {
            let timeout = self.call_budget(&slot.config);
            let cancel = Cancel::default();
            let catalog = client.tools_list(timeout, &cancel).await?;
            *state = SlotState::Connected {
                client: client.clone(),
                catalog,
            };
        }
        Ok(())
    }

    /// Invokes one tool, connecting on first use.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: serde_json::Value,
        cancel: &Cancel,
    ) -> Result<crate::protocol::CallToolResult, ManagerError> {
        let slot = self.slot(server)?;
        let handle = self.ensure_connected(server).await?;
        let timeout = self.call_budget(&slot.config);
        match handle
            .client
            .call_tool(tool, arguments, timeout, cancel)
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                // A dead connection marks the slot; the next caller
                // inside the cooldown fails fast instead of queueing
                // behind another doomed handshake.
                if handle.client.is_dead() {
                    self.mark_cooling(server, handle.client.stderr_tail()).await;
                }
                Err(ManagerError::Client(err))
            }
        }
    }

    /// Fires `cancel` against every in-flight call this manager has
    /// issued to `server`. The bridge calls this when a client
    /// disconnects mid-request; the manager holds no per-call
    /// registry, so cancellation rides the caller's own token.
    pub fn cancel_with(&self, _server: &str, cancel: &Cancel) {
        cancel.cancel();
    }

    fn slot(&self, server: &str) -> Result<&Arc<Slot>, ManagerError> {
        self.slots
            .get(server)
            .ok_or_else(|| ManagerError::UnknownServer(server.to_owned()))
    }

    /// The per-call budget: config may lower the default, never raise
    /// it — a longer call is moot while the invoking shell dies first.
    fn call_budget(&self, config: &McpServer) -> Duration {
        config.timeout.min(rhema_config::mcp::DEFAULT_TIMEOUT)
    }

    /// The single-flight connect. Locks the slot, returns the live
    /// connection or makes exactly one connect attempt; joiners behind
    /// the lock observe the outcome, never a second child.
    async fn ensure_connected(&self, server: &str) -> Result<Connected, ManagerError> {
        let slot = self.slot(server)?;
        let mut state = slot.state.lock().await;
        match &*state {
            SlotState::Connected { client, catalog } if !client.is_dead() => {
                return Ok(Connected {
                    client: client.clone(),
                    catalog: catalog.clone(),
                });
            }
            SlotState::Connected { client, .. } => {
                // Dead between calls: record the death and fail this
                // call in-band with the tail — the respawn belongs to
                // the first caller after the cooldown, not to whoever
                // happened to observe the death.
                let tail = client.stderr_tail();
                *state = SlotState::Cooling {
                    until: Instant::now() + self.cooldown,
                    tail: tail.clone(),
                };
                return Err(ManagerError::Cooling {
                    server: server.to_owned(),
                    tail,
                });
            }
            SlotState::Disconnected => {}
            SlotState::Cooling { until, tail } => {
                if *until > Instant::now() {
                    return Err(ManagerError::Cooling {
                        server: server.to_owned(),
                        tail: tail.clone(),
                    });
                }
            }
        }

        // Fresh connect attempt: the lock is held for the whole spawn
        // + handshake, so concurrent callers wait out the flight.
        let startup_timeout = slot.config.startup_timeout;
        let result = match &slot.config.transport {
            rhema_config::mcp::McpTransport::Stdio { command, env } => {
                let (program, args) = command
                    .split_first()
                    .ok_or(ClientError::Protocol("empty command"))?;
                Client::connect_stdio(
                    StdioParams {
                        program: program.clone(),
                        args: args.to_vec(),
                        env: env.clone(),
                    },
                    startup_timeout,
                )
                .await
            }
            rhema_config::mcp::McpTransport::Http { url, headers } => {
                Client::connect_http(HttpParams {
                    url: url.clone(),
                    headers: headers.clone(),
                })
                .await
            }
        };

        let client = match result {
            Ok(client) => Arc::new(client),
            Err(err) => {
                *state = SlotState::Cooling {
                    until: Instant::now() + self.cooldown,
                    tail: String::new(),
                };
                return Err(ManagerError::Client(err));
            }
        };

        // The catalog rides the connect: one tools/list walk per
        // connection, cached hereafter.
        let timeout = self.call_budget(&slot.config);
        let cancel = Cancel::default();
        let catalog = match client.tools_list(timeout, &cancel).await {
            Ok(catalog) => catalog,
            Err(err) => {
                let tail = client.stderr_tail();
                *state = SlotState::Cooling {
                    until: Instant::now() + self.cooldown,
                    tail,
                };
                return Err(ManagerError::Client(err));
            }
        };

        *state = SlotState::Connected {
            client: client.clone(),
            catalog: catalog.clone(),
        };
        Ok(Connected { client, catalog })
    }

    /// Marks a live slot cooling from now, keeping the tail. Called
    /// after a call observed its own connection dead.
    async fn mark_cooling(&self, server: &str, tail: String) {
        if let Some(slot) = self.slots.get(server) {
            let mut state = slot.state.lock().await;
            if let SlotState::Connected { .. } = &*state {
                *state = SlotState::Cooling {
                    until: Instant::now() + self.cooldown,
                    tail,
                };
            }
        }
    }
}

/// What [`McpManager::ensure_connected`] hands back: the live client
/// and the catalog it was born with.
struct Connected {
    client: Arc<Client>,
    catalog: Vec<ToolDef>,
}
