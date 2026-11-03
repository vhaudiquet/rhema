//! rhema-cloud-agent — the coding-agent harness the cloud frontends
//! share.
//!
//! A cloud frontend (a Durable Object, a container on a serverless
//! platform) needs the same three things regardless of vendor: an
//! agent assembled without a filesystem ([`AgentAssembly`]), approval
//! decisions that round-trip through the connected client
//! ([`ClientGate`]), and `bash` running in someone else's machine (the
//! [`Sandbox`] seam, [`ShellSandbox`] adapting it onto the shell
//! executor). This crate is that layer, next to its per-vendor
//! siblings — the seam crate of the provider family, applied to
//! frontends: [`rhema-cloudflare-code`](../rhema-cloudflare-code)
//! assembles from here and adds only the transport, the store, and the
//! glue its platform demands.
//!
//! The per-session configuration ([`SessionConfig`]) — model, base
//! prompt, approval policy, workspace, seed, skills, step cap — lives
//! here too: the vendor crates persist it wherever their platform
//! keeps durable bytes (a DO meta table) and hand it
//! to the assembly.
//!
//! # The sandbox seam
//!
//! The [`Sandbox`] contract, its request and outcome types, and
//! [`ShellSandbox`] live in
//! [`rhema-sandbox`](../rhema-sandbox), extracted so any frontend can
//! reach the seam without being born in this crate. This crate
//! re-exports them so its public surface does not churn while callers
//! migrate; the re-exports go once every caller imports the seam crate
//! directly (`ScriptedSandbox`, the seam's offline test double, rides
//! along under the `mock` feature).
//!
//! # Module hygiene
//!
//! No platform dependency enters this crate: no wasm-bindgen, no
//! `worker`, no HTTP client. Everything compiles and tests on any
//! host, and every module carries `#![forbid(unsafe_code)]`. The
//! offline end-to-end (`tests/e2e.rs`, feature `mock`) drives a full
//! turn — a bash call through the approval round-trip, a denial, a
//! crash mid-command and its resume — with the scripted sandbox and
//! the scripted provider; a frontend inherits the guarantees by using
//! the same pieces.
//!
//! # Non-goals
//!
//! Durability lives in the engine's commit hooks over a
//! [`rhema_rsd::Store`](https://docs.rs/rhema-rsd) implementation; this
//! crate owns no storage. Conversation topology (forks, per-session
//! agent beyond this config) waits for a product shape that wants it.
#![forbid(unsafe_code)]

pub mod assembly;
pub mod config;
pub mod gate;
pub mod wire;

pub use assembly::{AgentAssembly, ApprovalPolicy, PolicyApproval};
pub use config::{SessionConfig, SkillSeed};
pub use gate::{ApprovalChannel, ClientGate};
// The sandbox seam lives in rhema-sandbox; re-exported so this crate's
// public surface does not churn during the transition.
pub use rhema_sandbox::{
    ExecOutcome, ExecRequest, Sandbox, SandboxError, ShellSandbox, WorkspaceSeed,
};
pub use wire::{ClientMessage, ServerMessage};

#[cfg(feature = "mock")]
pub use rhema_sandbox::mock;
#[cfg(feature = "mock")]
pub use rhema_sandbox::mock::ScriptedSandbox;
