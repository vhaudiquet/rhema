//! `rhema-autonomous-agent` — the resident-agent core tier
//! (specs/rhema-framework-v1.md): the source-driven session loop, the
//! `Source` contract for deterministic detection, and a tool registry
//! with allowlists, all over `rhema-rsd`'s dispatcher.
//!
//! The crate knows sources, sessions, tools, and a driver — no specific
//! integration. A [**Source**](source::Source) detects (never reasons):
//! its `detect` pass is deterministic code that polls the outside world
//! and ingests events; the model is only ever reached inside a
//! [**Handler**](driver::Handler) run, after the dispatcher granted a
//! lease. A [**Driver**](driver::Driver) turns admitted keys into handler
//! runs — acquire, live inbox, checkpoint, release — the spec §3 loop
//! with `poll_inbox` batched at step boundaries and a checkpoint per
//! round. The [**registry**](registry) aggregates named
//! [`Tool`](registry::Tool)s into a `rhema_core::ToolResolver` filtered
//! by an [`Allowlist`](allowlist::Allowlist) fixed before the handler's
//! first step (spec §8).
//!
//! Deliberately deferred, with the reasoning recorded where the decision
//! bites: classification profiles (`ClassifyFn`) stay at the source
//! boundary — `rhema-rsd` ships without them (its documented §9
//! deviation), and no second consumer needs the shared vocabulary yet;
//! allowlist *policy* (which profile gets which tools) is per-deployment
//! configuration, resolved by the consumer before it builds its agent.
//!
//! Layout:
//! - [`allowlist`] — `Allowlist`: the permissive default and tool-name sets
//! - [`chain`] — `ResolverChain`: several resolvers behind one seam
//! - [`registry`] — the `Tool` contract and the allowlist-filtering `ToolRegistry`
//! - [`source`] — the `Source` contract: deterministic detection into the dispatcher
//! - [`driver`] — the `Driver`, `Handler`, and `SessionContext`: the lease-driven loop

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod allowlist;
pub mod chain;
pub mod driver;
pub mod registry;
pub mod source;

// Re-exported so implementors of the async seams can apply #[async_trait]
// without adding the crate themselves.
pub use allowlist::Allowlist;
pub use async_trait::async_trait;
pub use chain::ResolverChain;
pub use driver::{Driver, Handler, SessionContext};
pub use registry::{Tool, ToolRegistry};
pub use source::{Source, SourceError};
