//! rhema-coding-agent — the coding-agent driver, the first consumer of
//! rhema-core's step engine.
//!
//! Wires a [`ModelProvider`](rhema_core::ModelProvider) into a
//! [`CodingAgent`] whose turns drive `rhema_core::run_turn` against a
//! built-in tool set: one approval-gated shell (`bash`), with skills
//! presented in the system prompt for the model to load via the shell.
//! Tool errors surface in-band to the model (SPEC.md §4); approval
//! arrives per turn through the [`Approval`] hook; the session is plain
//! data the driver can persist and resume.
//!
//! Layout:
//! - [`approval`] — the `Approval` hook and `AllowAll`
//! - [`agent`] — `CodingAgent`, `CodingAgentError`, `render_system`
//! - [`config`] — `CodingAgentConfig`
//! - [`skills`] — discovery of markdown skills, prompt presentation
//! - [`tools`] — `CodingTools`, the built-in `ToolResolver`
//!
//! # Skills
//!
//! A skill is a folder with a `SKILL.md` whose frontmatter carries a
//! `description` (the trigger). Only the name / trigger / file table
//! enters the system prompt; the model reads a body itself through the
//! bash tool when a trigger applies. Skills live globally under the
//! config directory and per-workspace under `.rhema/skills`, project
//! shadowing global.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod agent;
pub mod approval;
pub mod config;
pub mod skills;
pub mod tools;

pub use agent::{CodingAgent, CodingAgentError, DEFAULT_SYSTEM_MD, default_system};
pub use approval::{AllowAll, Approval, DenyAll};
pub use config::CodingAgentConfig;
pub use skills::Skill;
pub use tools::CodingTools;
