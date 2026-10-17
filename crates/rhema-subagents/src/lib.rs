//! rhema-subagents — the subagent extension.
//!
//! Delegation as one tool. A subagent is a fresh
//! [`Agent`](rhema_core::Agent) on a fresh
//! [`Session`](rhema_core::Session): its own system prompt, its own
//! workspace tools, a bounded step loop — and its final text comes back
//! as the tool result. The parent never sees the subagent's history,
//! only the report, and the report lands in the parent's history like
//! any tool result, so commit-before-execute, revive-on-resume, and
//! cancel-path repair keep working unchanged (see
//! `specs/rhema-subagents.md`).
//!
//! Layout:
//! - [`def`] — `SubagentDef`, discovery over markdown definitions
//!
//! The crate is an extension: it depends on the existing stack and
//! nothing depends on it. `rhema-core` is untouched.
//!
//! # Definitions
//!
//! A subagent is a folder with a `SUBAGENT.md` whose frontmatter carries
//! a `description` (the trigger) and whose body is the subagent's system
//! prompt. Definitions live globally under the config directory and
//! per-workspace under `.rhema/subagents`, project shadowing global —
//! the same rules as skills.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod def;

pub use def::{SubagentDef, discover};
