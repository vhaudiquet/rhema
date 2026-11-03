//! rhema-cloudflare-code — the coding agent as a Cloudflare Durable
//! Object.
//!
//! One [`worker::CodingAgent`](worker::CodingAgent) instance per
//! session id. The agent itself is the ordinary
//! [`CodingAgent`](rhema_coding_agent::CodingAgent) driver over the
//! ordinary step engine — this crate is a frontend, next to the
//! terminal and the ACP adapter. The frontend-independent harness —
//! the filesystem-less assembly, the client approval gate, the sandbox
//! seam, the per-session config — lives in
//! [`rhema-cloud-agent`](../rhema-cloud-agent); this crate adds only
//! what Cloudflare demands:
//!
//! - the model rides [`worker::FetchTransport`], a
//!   [`Transport`](rhema_provider::Transport) over workerd's host
//!   fetch, bridging the `!Send` JS byte stream into plain chunks;
//! - `bash` runs in a Cloudflare Sandbox container bound to the same
//!   session name, reached through the shared
//!   [`Sandbox`](rhema_cloud_agent::Sandbox) seam
//!   ([`worker::SandboxDo`] implements it) — or, when the deployment
//!   sets `SANDBOX_BACKEND=http`, on any machine speaking the sandbox
//!   protocol, through the shared HTTP sandbox client over host fetch
//!   (the selection lives in [`backend`]);
//! - the session checkpoints into the DO's SQLite storage — at the
//!   commit hook, before every side effect, and after each turn —
//!   revision-guarded, through a
//!   [`rhema_rsd::Store`](rhema_rsd::Store) implementation, so any
//!   eviction resumes through the engine's existing revive machinery;
//! - the per-session [`SessionConfig`](rhema_cloud_agent::SessionConfig)
//!   persists in the DO's meta table, set over `POST /config`.
//!
//! # Module hygiene
//!
//! The platform lives at the edges, in the `worker` module and nowhere
//! else: wasm-bindgen, `worker`, and host-fetch dependencies are absent
//! from the harness — which moved to `rhema-cloud-agent` precisely so
//! the compiler polices that boundary. What stays here compiles only
//! on wasm32 (feature `worker`).
//!
//! The crate as a whole carries `#![deny(missing_docs)]` but **not**
//! `#![forbid(unsafe_code)]`: the `worker` module's `#[durable_object]`
//! macro emits `unsafe` internally, and a crate-level `forbid` cannot
//! be overridden by an inner `allow`. That exception is sanctioned and
//! documented here (the precedent: `vendor/crossterm` is excluded from
//! the workspace for toolchain reasons).
//!
//! # Non-goals (v1)
//!
//! The terminal UI, the claw daemon, and the local-only provider stack
//! do not port to wasm. One provider adapter (Anthropic). Memory tools
//! and the subagents extension wait for storage-backed discovery; the
//! alarm-driven step queue is a v2 refinement of turn driving.

pub mod backend;

pub use rhema_cloud_agent::{
    AgentAssembly, ApprovalChannel, ApprovalPolicy, ClientGate, ExecOutcome, ExecRequest,
    PolicyApproval, Sandbox, SandboxError, SessionConfig, ShellSandbox, SkillSeed, WorkspaceSeed,
};

#[cfg(all(feature = "worker", target_arch = "wasm32"))]
pub mod worker;
