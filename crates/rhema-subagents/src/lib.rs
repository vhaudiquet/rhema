//! rhema-subagents — the subagent extension.
//!
//! Delegation as one tool with two shapes. `subagent(name, prompt)`
//! **starts** a subagent — a fresh [`Agent`](rhema_core::Agent) on a
//! fresh [`Session`](rhema_core::Session): its own system prompt, its
//! own workspace tools, a bounded step loop — in the background and
//! returns a job id at once; the job's report is announced to the model
//! by an automatic notification the driver appends, and the model
//! fetches it by calling `subagent(id)`. The parent never sees the
//! subagent's history, only the report, and both the start result and
//! the fetched report land in the parent's history like any tool
//! result, so commit-before-execute, revive-on-resume, and cancel-path
//! repair keep working unchanged (see `specs/rhema-subagents.md` and
//! SPEC.md §13).
//!
//! Jobs live in a [`registry`](crate::registry::Registry): ids, states,
//! the capped report cache, one cancel flag per job (escape stops the
//! main session, never a running subagent), and a concurrency cap
//! past which starts queue. The registry is also the two read surfaces:
//! the status subscription the UI renders one live line per job from,
//! and the notifications the driver drains at step boundaries. Jobs are
//! bound to the agent's lifetime — dropped with it, reports lost.
//!
//! Layout:
//! - [`def`] — `SubagentDef`, discovery over markdown definitions
//! - [`tools`] — `SubagentTools`, the `subagent` tool and the inner run
//! - [`registry`] — the job registry: ids, states, the cap, the
//!   subscription
//! - [`wire`] — the explicit `CodingAgentConfig` helper frontends call
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
//! the same rules as skills. The catalog also carries a built-in:
//! `general-purpose`, the delegation every agent gets, shadowed by a
//! definition of the same name.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod def;
pub mod registry;
pub mod tools;
pub mod wire;

pub use def::{SubagentDef, builtin, discover};
pub use registry::JobStatus;
pub use tools::SubagentTools;
pub use wire::{SubagentHandle, extra_specs, wire, wire_at};
