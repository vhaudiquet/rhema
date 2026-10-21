//! rhema-cloudflare-code — the coding agent as a Cloudflare Durable
//! Object.
//!
//! One [`worker::CodingAgent`](worker::CodingAgent) instance per
//! session id. The agent itself is the ordinary
//! [`CodingAgent`](rhema_coding_agent::CodingAgent) driver over the
//! ordinary step engine — this crate is a *frontend*, the third one,
//! next to the terminal and the ACP adapter. What the cloud changes is
//! where things live:
//!
//! - the model rides [`worker::FetchTransport`], a
//!   [`Transport`](rhema_provider::Transport) over workerd's host
//!   fetch, bridging the `!Send` JS byte stream into plain chunks;
//! - `bash` runs in a Cloudflare Sandbox container bound to the same
//!   session name, reached through the [`Sandbox`] seam
//!   ([`ShellSandbox`] adapts it onto the shell executor);
//! - the session checkpoints into the DO's SQLite storage after every
//!   step, revision-guarded, through a
//!   [`rhema_rsd::Store`](rhema_rsd::Store) implementation, so any
//!   eviction resumes through the engine's existing revive machinery;
//! - approvals are a per-session policy
//!   ([`ApprovalPolicy`]) plus, when a client is connected, a WebSocket
//!   round-trip with deny-on-timeout.
//!
//! # Module hygiene
//!
//! The platform lives at the edges, in the `worker` module and nowhere
//! else: wasm-bindgen, `worker`, and host-fetch dependencies are absent
//! from [`sandbox`], [`shell`], and [`assembly`], which compile and
//! test on any host. Every platform-neutral module carries its own
//! `#![forbid(unsafe_code)]`.
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

#![deny(missing_docs)]

pub mod assembly;
pub mod gate;
pub mod sandbox;
pub mod shell;

#[cfg(feature = "mock")]
pub mod mock;

#[cfg(all(feature = "worker", target_arch = "wasm32"))]
pub mod worker;

pub use assembly::{AgentAssembly, ApprovalPolicy, PolicyApproval};
pub use gate::{ApprovalChannel, ClientGate};
pub use sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
pub use shell::ShellSandbox;
