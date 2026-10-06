//! rhema-coding-agent — the coding-agent driver, the first consumer of
//! rhema-core's step engine.
//!
//! Wires a [`ModelProvider`](rhema_core::ModelProvider) into a
//! [`CodingAgent`] whose turns drive `rhema_core::run_turn` against a
//! built-in tool set: file inspection and editing scoped to a workspace,
//! plus an approval-gated shell command. Tool errors surface in-band to
//! the model (SPEC.md §4); approval arrives per turn through the
//! [`Approval`] hook; the session is plain data the driver can persist and
//! resume.
//!
//! Layout:
//! - [`approval`] — the `Approval` hook and `AllowAll`
//! - [`agent`] — `CodingAgent`, `CodingAgentError`, `default_system`
//! - [`config`] — `CodingAgentConfig`
//! - [`tools`] — `CodingTools`, the built-in `ToolResolver`

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod agent;
pub mod approval;
pub mod config;
pub mod tools;

pub use agent::{CodingAgent, CodingAgentError, default_system};
pub use approval::{AllowAll, Approval};
pub use config::CodingAgentConfig;
pub use tools::CodingTools;
