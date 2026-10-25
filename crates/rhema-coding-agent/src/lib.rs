//! rhema-coding-agent — the coding-agent driver, the first consumer of
//! rhema-core's step engine.
//!
//! Wires a [`ModelProvider`](rhema_core::ModelProvider) into a
//! [`CodingAgent`] whose turns drive `rhema_core::run_turn` over the
//! shared toolkit (`rhema-agent-tools`): the approval-gated shell, its
//! spec and discovered skills presented in the system prompt, the model
//! loading skill bodies via the shell. Tool errors surface in-band to
//! the model (SPEC.md §4); approval arrives per turn through the
//! [`Approval`] hook; the session is plain data the driver can persist
//! and resume, with a commit hook so an intent is durable before its
//! first side effect, revive-on-resume for calls a crash left
//! unanswered, and cancel-path repair for those a user stopped.
//!
//! Layout:
//! - [`agent`] — `CodingAgent`, `CodingAgentError`, `stamped_system`
//! - [`config`] — `CodingAgentConfig` and the `Steering` seam
//! - [`prompt`] — the SYSTEM.md template and its default render
//! - [`environment`] — the startup environment report and the sandbox
//!   notice, the prompt's environment section
//!
//! The toolkit itself — the `Approval` seam, `ShellTools`, skills, paste
//! expansion, dangling-call repair, the renderer — lives in
//! `rhema-agent-tools` and re-exports here: the TUI and ACP drivers
//! import these names from this crate, and the claw's forge handler
//! takes the approval policy the same way.
//!
//! # Skills
//!
//! A skill is a folder with a `SKILL.md` whose frontmatter carries a
//! `description` (the trigger). Only name, trigger, and file enter the
//! system prompt, as the [`prompt`] module's `skills` list; the model
//! reads a body itself through the bash tool when a trigger applies.
//! Skills live globally under the config directory and
//! per-workspace under `.rhema/skills`, project shadowing global.

#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
pub mod environment;
pub mod prompt;

pub use agent::{
    CodingAgent, CodingAgentError, clear_environment_stamp, stamped_system,
    stamped_system_with_sections, stamped_system_with_skills,
};
pub use config::{CodingAgentConfig, Steering};
pub use environment::{host_report, remote_notice};
pub use prompt::{DEFAULT_SYSTEM_MD, default_system};
// The toolkit the driver is built on, under this crate's name: every
// downstream driver (TUI, ACP, the claw) keeps its import paths.
pub use rhema_agent_tools::{
    AllowAll, Approval, DenyAll, ExpandPastes, ShellTools, render_system, render_template,
    repair_dangling_calls, revive_dangling_calls,
};
