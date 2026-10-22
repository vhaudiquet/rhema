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
//! frontends: [`rhema-cloudflare-code`](../rhema-cloudflare-code) and
//! `rhema-vercel-agent` assemble from here and add only the transport,
//! the store, and the glue their platform demands.
//!
//! The per-session configuration ([`SessionConfig`]) — model, base
//! prompt, approval policy, workspace, seed, skills, step cap — lives
//! here too: the vendor crates persist it wherever their platform
//! keeps durable bytes (a DO meta table, a Postgres row) and hand it
//! to the assembly.
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
#![deny(missing_docs)]

pub mod assembly;
pub mod config;
pub mod gate;
pub mod sandbox;
pub mod shell;

#[cfg(feature = "mock")]
pub mod mock;

pub use assembly::{AgentAssembly, ApprovalPolicy, PolicyApproval};
pub use config::{SessionConfig, SkillSeed};
pub use gate::{ApprovalChannel, ClientGate};
pub use sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
pub use shell::ShellSandbox;
