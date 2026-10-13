//! `rhema-claw` — the resident agent daemon: forge watches plus the chat
//! HTTP surface, the first consumer of `rhema-autonomous-agent`.
//!
//! The spec invariants this crate exists to keep (specs/
//! rhema-framework-v1.md): detection is deterministic code, never the
//! model (§1) — every timer here drives a detect pass and an admission
//! step, and the model is reached only inside a handler run; there is no
//! periodic, unconditional model-invoking timer (§7) — a scheduled task
//! is an event the [`TaskScheduler`] detects, not a heartbeat; and the
//! open interactive surface never resolves keys (§6.2) — one resident
//! key is one owned session, forever. Two dispatchers share one SQLite
//! store: the forge dispatcher turns watch activity into [`ClawHandler`]
//! runs (one turn per dispatch, the final text posted back as the
//! thread's comment); the resident dispatcher — keys prefixed
//! `interactive:`, `task:`, or `phone:`, the contract
//! [`interactive`] documents — turns chat messages, `/v1/ingest`
//! events, and due tasks into [`InteractiveHandler`] runs (one turn per
//! dispatch, streamed to a chat subscriber or auto-notified to a
//! device).
//!
//! Layout:
//! - [`config`] — the YAML config: the watch seed, paths, HTTP surface,
//!   provider and model
//! - [`bootstrap`] — the SOUL/USER/ENVIRONMENT store, the system prompt
//!   rendered from it per dispatch with the dispatch's tool roster, and
//!   the `bootstrap_save` tool
//! - [`watch_store`] — the persistent watch table (SQLite, shared file)
//! - [`notifications`] — the notification table: the outbound queue the
//!   devices poll and ack
//! - [`task_store`] — the task table: scheduled prompts, inert until the
//!   scheduler detects them due
//! - [`turn_store`] — the turn-history table: what each key's stream
//!   streamed, flushed per turn, replayed after a restart
//! - [`hub`] — the event hub: per-key record and broadcast of turn events
//! - [`identity`] — the agent's presentation identity: name, mark,
//!   images, served over `/v1/agent`
//! - [`watch_tools`] — the agent's watch-management tool set
//! - [`notify_tools`] — the `notify` tool: the queue a device polls
//! - [`task_tools`] — `schedule_task`, `list_tasks`, `cancel_task`
//! - [`forge`] — the sources: one detectable stream per forge kind
//! - [`task_source`] — the [`TaskScheduler`]: due tasks in, events out
//! - [`handler`] — the forge dispatch: snapshot, inbox, one turn, one
//!   comment
//! - [`interactive`] — the resident dispatch: the key-prefix contract,
//!   inbox, one turn, one streamed reply, auto-notify
//! - [`http`] — the routes: chat, sessions, watches, ingest,
//!   notifications, the event stream
//! - [`openai`] — the `/v1/chat/completions` shim for stock web UIs
//! - [`daemon`] — the assembly and the loop (`run` with HTTP, and the
//!   headless `--once` shape [`run_once`])

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod accounts_tools;
pub mod backends;
pub mod bootstrap;
pub mod config;
pub mod daemon;
pub mod environment;
pub mod forge;
pub mod handler;
pub mod http;
pub mod hub;
mod identity;
pub mod interactive;
#[cfg(feature = "mock")]
pub mod mock;
pub mod notifications;
pub mod notify_tools;
pub mod openai;
pub mod settings;
pub mod task_source;
pub mod task_store;
pub mod task_tools;
pub mod triage;
pub mod turn_store;
pub mod watch_store;
pub mod watch_tools;

pub use backends::{Backend, BackendRegistry};
pub use bootstrap::{
    BOOTSTRAP_SAVE, Bootstrap, BootstrapError, BootstrapKind, BootstrapStatus, DEFAULT_SYSTEM_MD,
    render_system,
};
pub use config::{ClawConfig, ConfigError, ForgeKind, Watch};
pub use daemon::{ClawError, Daemon, TickReport, run_once};
pub use environment::write_report;
pub use forge::ForgeSource;
pub use handler::ClawHandler;
pub use hub::{EventHub, TurnEvent};
pub use identity::{AgentIdentity, IdentityError, IdentityStore, ImageKind};
pub use interactive::InteractiveHandler;
#[cfg(feature = "mock")]
pub use mock::MockLoop;
pub use notifications::{NotificationRecord, NotificationStore, NotificationStoreError};
pub use settings::SettingsStore;
pub use task_source::{TASKS_SOURCE, TaskScheduler};
pub use task_store::{TaskKind, TaskRecord, TaskStore, TaskStoreError};
pub use turn_store::{TurnStore, TurnStoreError};
pub use watch_store::{ClawStore, WatchRecord, WatchStoreError};
