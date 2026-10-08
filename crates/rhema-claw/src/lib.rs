//! `rhema-claw` — the resident forge-watching agent daemon, the first
//! consumer of `rhema-autonomous-agent`.
//!
//! The spec invariants this crate exists to keep (specs/
//! rhema-framework-v1.md): detection is deterministic code, never the
//! model (§1) — every timer here drives a detect pass and an admission
//! step, and the model is reached only inside a handler run; there is no
//! periodic, unconditional model-invoking timer (§7). One [`Source`] per
//! forge kind polls the watches into the dispatcher; the [`Daemon`]'s
//! [`Driver`](rhema_autonomous_agent::Driver) turns admitted keys into
//! [`ClawHandler`] runs — one coding-agent turn per dispatch, the final
//! text posted back as the thread's comment, the session stashed into the
//! dispatcher's persistent state so a resumed thread keeps its
//! conversation.
//!
//! Layout:
//! - [`config`] — the YAML config: watches, paths, provider and model
//! - [`forge`] — the sources: one detectable stream per forge kind
//! - [`handler`] — the dispatch: snapshot, inbox, one turn, one comment
//! - [`daemon`] — the assembly and the loop (`run`, and the `--once`
//!   shape [`run_once`])

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod config;
pub mod daemon;
pub mod forge;
pub mod handler;

pub use config::{ClawConfig, ConfigError, ForgeKind, Watch};
pub use daemon::{ClawError, Daemon, TickReport, run_once};
pub use forge::ForgeSource;
pub use handler::ClawHandler;
