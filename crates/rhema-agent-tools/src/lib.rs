//! rhema-agent-tools — the agent toolkit on rhema-core's tool
//! abstraction.
//!
//! The pieces any agent needs, concrete and shared. The concepts stay in
//! rhema-core (`ToolResolver`, `ToolCall`, `Session`); this crate
//! implements them:
//!
//! - [`approval`] — the `Approval` hook and `AllowAll`/`DenyAll`
//! - [`exec`] — the `ShellExecutor` seam: where `bash` commands run
//! - [`shell`] — `ShellTools`, one approval-gated `bash` scoped to a
//!   workspace; reading, listing, and editing are shell work, not
//!   separate tools
//! - [`skills`] — discovery of markdown skills, prompt presentation
//! - [`overlay`] — `Overlay`, one resolver over built-ins plus an extension set
//! - [`paste`] — `ExpandPastes`, paste expansion at the provider seam
//! - [`revive`] — `repair_dangling_calls`, `revive_dangling_calls`
//! - [`prompt`] — the Jinja system-prompt renderer
//! - [`vision`] — the `see` macro: a bash command's staged images
//!   become [`ToolResult`] image blocks
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

pub mod approval;
pub mod exec;
pub mod overlay;
pub mod paste;
pub mod prompt;
pub mod revive;
pub mod shell;
pub mod skills;
pub mod vision;

pub use approval::{AllowAll, Approval, DenyAll};
#[cfg(feature = "local-shell")]
pub use exec::LocalShell;
pub use exec::{Executed, ShellError, ShellExecutor};
pub use overlay::Overlay;
pub use paste::ExpandPastes;
pub use prompt::{render_system, render_system_with_sections, render_template};
// Re-exported so a caller can name the error the renderer returns
// without depending on minijinja itself.
pub use prompt::TemplateError;
pub use revive::{repair_dangling_calls, revive_dangling_calls};
pub use shell::ShellTools;
pub use skills::Skill;
