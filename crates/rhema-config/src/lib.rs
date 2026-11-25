//! The settings contract between rhema and its frontends.
//!
//! rhema is a framework: the agent driver, the providers, the sandbox
//! and MCP wiring, and any number of frontends built on top, each one
//! owning the user-facing product. This crate is what they agree on:
//! the [`Settings`] every frontend must be able to store and read,
//! whatever file, directory, or format it keeps them in; the [`Store`]
//! seam a frontend implements to put them wherever it likes; the
//! [`Home`] of directory conventions; and the [`secrets`] seam, whose
//! encrypted file backend lives under each frontend's own home, so two
//! frontends never share credentials by accident.
//!
//! The reference frontend keeps its whole file, the [`Settings`]
//! section beside its own UI section, at
//! `$XDG_CONFIG_HOME/rhema/config.yaml`, written with `0600`
//! permissions since the section can hold credentials. A different
//! frontend keeps them under its own directory, in its own format: it
//! embeds [`Settings`] as one serde field, implements [`Store`], and
//! the framework itself reads no path and parses no format. See
//! `examples/godcode.rs` for that story in one runnable file.
//!
//! Which profile a session runs under is chosen per invocation
//! (`--profile`) or per session (`/profile`), it is never stored as a
//! global switch, so parallel instances (work and personal side by
//! side) never fight over one toggle.
//!
//! A profile holds any number of named [`ProviderInstance`]s, two
//! llama.cpp servers, a work and a personal Copilot sub, with one of
//! them active (`provider` + `instance`).
//!
//! A key lives in one of two places: the encrypted secret store, which
//! the settings reference by an opaque `secret_ref` ([`secrets`]), or
//! the store itself as the literal `api_key`, the fallback when the
//! secret store refuses. [`Profile::place_key`] and
//! [`Settings::migrate_secrets`] prefer the secret store and only fall
//! back to the literal when it refuses; resolution reads the store
//! first on every tier. A machine upgrading from the old OS keyring
//! copies its entries one way ([`Settings::import_legacy_secrets`]);
//! nothing writes back there.
//!
//! Environment variables (`RHEMA_PROVIDER`, `GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, `ZAI_API_KEY`,
//! `GITHUB_COPILOT_TOKEN`, `RHEMA_SANDBOX`, `RHEMA_SANDBOX_URL`,
//! `RHEMA_SANDBOX_TOKEN`) always win over the stored value, a stored
//! entry is the fallback, not the override.

#![forbid(unsafe_code)]

pub mod privacy;
pub mod secrets;

mod catalog;
mod error;
mod home;
mod instances;
mod platform;
mod profile;
mod settings;
mod store;

pub use catalog::*;
pub use error::*;
pub use home::*;
pub use instances::*;
pub use platform::*;
pub use profile::*;
pub use settings::*;
pub use store::*;
