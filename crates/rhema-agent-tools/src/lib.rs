//! rhema-agent-tools — the agent toolkit on rhema-core's tool
//! abstraction.
//!
//! The pieces any agent needs, concrete and shared. The concepts stay in
//! rhema-core (`ToolResolver`, `ToolCall`, `Session`); this crate
//! implements them:
//!
//! - [`approval`] — the `Approval` hook and `AllowAll`/`DenyAll`
//! - [`shell`] — `ShellTools`, one approval-gated `bash` scoped to a
//!   workspace; reading, listing, and editing are shell work, not
//!   separate tools
//! - [`skills`] — discovery of markdown skills, prompt presentation
//! - [`paste`] — `ExpandPastes`, paste expansion at the provider seam
//! - [`revive`] — `repair_dangling_calls`, `revive_dangling_calls`
//! - [`prompt`] — the Jinja system-prompt renderer
//!
//! # Skills
//!
//! A skill is a folder with a `SKILL.md` whose frontmatter carries a
//! `description` (the trigger). Only name, trigger, and file enter the
//! system prompt; the model reads a body itself through the shell when a
//! trigger applies. Skills live globally under the config directory and
//! per-workspace under `.rhema/skills`, project shadowing global.
//!
//! # Consumers
//!
//! The coding-agent driver (`rhema-coding-agent`) builds on the kit; the
//! TUI and ACP adapters reach it through that driver. The claw's resident
//! agent assembles its own agent from the same pieces, which is the
//! point: one shell, one approval seam, one expansion, one renderer —
//! not one per driver.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod approval;
pub mod paste;
pub mod prompt;
pub mod revive;
pub mod shell;
pub mod skills;

pub use approval::{AllowAll, Approval, DenyAll};
pub use paste::ExpandPastes;
pub use prompt::{render_system, render_template};
pub use revive::{repair_dangling_calls, revive_dangling_calls};
pub use shell::ShellTools;
pub use skills::Skill;
