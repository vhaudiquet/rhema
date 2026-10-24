//! rhema-vercel-agent — the coding agent on Vercel.
//!
//! The second cloud frontend, over the harness
//! [`rhema-cloud-agent`](../rhema-cloud-agent) shares with the
//! Cloudflare DO: an ordinary axum server (a Vercel container runs it
//! natively — no JS shim, no `!Send` bridge), `bash` in a Vercel
//! Sandbox, sessions checkpointed into Postgres. The client protocol
//! is the DO's, byte for byte; the engine, the assembly, the approval
//! gate, and the per-session config are the same code.
//!
//! - [`api`] — the Vercel Sandbox REST client behind a seam, so the
//!   offline tests run without a token;
//! - [`sandbox`] — the [`Sandbox`](rhema_cloud_agent::Sandbox) seam
//!   over it: a named, persistent sandbox per session, commands
//!   polled to completion, files moved through the fs endpoints;
//! - [`backend`] — the deployment's sandbox backend choice: `bash` in
//!   a Vercel Sandbox (the default) or on any machine speaking the
//!   sandbox protocol, through the shared HTTP sandbox client;
//! - [`pg`] — the [`rhema_rsd::Store`] contract in Postgres, plus the
//!   meta table the per-session config and the seed marker live in;
//! - [`server`] — the routes and the turn loop: the DO's shape with
//!   real tasks where the isolate had one thread.
//!
//! What the platform changes and what it does not: the workspace
//! survives idle periods (persistence is the platform default), a
//! full region failure drops it (the next session re-seeds, the same
//! recovery the DO's eviction path takes), and the durable state
//! never leaves Postgres — the same one-contract shape the DO's
//! SQLite keeps.
//!
//! # Non-goals (v1)
//!
//! One provider adapter (Anthropic). No VCR image build in-tree (the
//! deploy README covers it). The alarm-driven queue that would let
//! turns outlive connections is the same v2 refinement the DO awaits.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod api;
pub mod backend;
pub mod pg;
pub mod sandbox;
pub mod server;

pub use api::{ApiError, ReqwestSandboxApi, SandboxApi};
pub use pg::PgStore;
pub use sandbox::VercelSandbox;
pub use server::{AppState, SandboxSource, router};

// The binary lives in main.rs; the library exposes everything it
// wires together.
