//! `rhema-claw` — the resident agent daemon: forge watches plus the chat
//! HTTP surface, the first consumer of `rhema-autonomous-agent`.
//!
//! The spec invariants this crate exists to keep (specs/
//! rhema-framework-v1.md): detection is deterministic code, never the
//! model (§1) — every timer here drives a detect pass and an admission
//! step, and the model is reached only inside a handler run; there is no
//! periodic, unconditional model-invoking timer (§7); and the open
//! interactive surface never resolves keys (§6.2) — one
//! `interactive:<name>` key is one owned session, forever. Two
//! dispatchers share one SQLite store: the forge dispatcher turns watch
//! activity into [`ClawHandler`] runs (one turn per dispatch, the final
//! text posted back as the thread's comment), the interactive
//! dispatcher turns chat messages into [`InteractiveHandler`] runs (one
//! turn per dispatch, the final text streamed to the subscriber).
//!
//! Layout:
//! - [`config`] — the YAML config: the watch seed, paths, HTTP surface,
//!   provider and model
//! - [`bootstrap`] — the SOUL/USER/ENVIRONMENT store, the system prompt
//!   rendered from it per dispatch, and the `bootstrap_save` tool
//! - [`watch_store`] — the persistent watch table (SQLite, shared file)
//! - [`hub`] — the event hub: per-key broadcast of live turn events
//! - [`watch_tools`] — the agent's watch-management tool set
//! - [`forge`] — the sources: one detectable stream per forge kind
//! - [`handler`] — the forge dispatch: snapshot, inbox, one turn, one
//!   comment
//! - [`interactive`] — the interactive dispatch: inbox, one turn, one
//!   streamed reply
//! - [`http`] — the routes: chat, sessions, watches, the event stream
//! - [`openai`] — the `/v1/chat/completions` shim for stock web UIs
//! - [`daemon`] — the assembly and the loop (`run` with HTTP, and the
//!   headless `--once` shape [`run_once`])

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod bootstrap;
pub mod config;
pub mod daemon;
pub mod environment;
pub mod forge;
pub mod handler;
pub mod http;
pub mod hub;
pub mod interactive;
#[cfg(feature = "mock")]
pub mod mock;
pub mod openai;
pub mod watch_store;
pub mod watch_tools;

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
pub use interactive::InteractiveHandler;
#[cfg(feature = "mock")]
pub use mock::MockLoop;
pub use watch_store::{ClawStore, WatchRecord, WatchStoreError};
