//! rhema-bash, the bash tool and the macros that ride it.
//!
//! The model gets one tool: a command. Everything else in this crate is
//! reachable through that command, because a shell is where an agent
//! reads files, runs tests, and edits a tree, and a provisioned function
//! costs the tool list nothing:
//!
//! - [`approval`]: the `Approval` hook and `AllowAll`/`DenyAll`
//! - [`exec`]: the `ShellExecutor` seam: where `bash` commands run
//! - [`shell`]: `ShellTools`, one approval-gated `bash` scoped to a
//!   workspace; reading, listing, and editing are shell work, not
//!   separate tools
//! - [`vision`]: the `see` macro: a bash command's staged images
//!   become [`ToolResult`](rhema_core::ToolResult) image blocks
//! - [`jobs`]: the `job` macro: background shells the model starts now
//!   and reads on a later turn
//! - [`websearch`]: the `search` macro: ranked web results as shell
//!   output, providers per profile tried in order
//! - [`decision`]: the `decide` macro: a decision model's typed
//!   answers as shell output, one provider and model per profile
//! - [`init`]: the one `BASH_ENV` init-file provisioner the macros
//!   and the MCP bridge share
//! - [`runtime`]: the kit description and the one init-file generator
//!   a helper writes inside a sandbox
//!   (`specs/rhema-runtime-kit.md`)
//!
//! # Consumers
//!
//! The host-agent driver (`rhema-host-agent`) builds on the crate;
//! the TUI and ACP adapters reach it through that driver. The claw's
//! resident agent assembles its own agent from the same pieces, which is
//! the point: one shell, one approval seam, one macro wiring, not one
//! per driver.
#![forbid(unsafe_code)]

pub mod approval;
pub mod decision;
pub mod exec;
pub mod init;
pub mod jobs;
pub mod placement;
pub mod runtime;
pub mod shell;
pub mod vision;
pub mod websearch;

pub use approval::{AllowAll, Approval, DenyAll};
#[cfg(feature = "local-shell")]
pub use exec::LocalShell;
pub use exec::{RunOutcome, RunRequest, ShellError, ShellExecutor};
pub use shell::ShellTools;
