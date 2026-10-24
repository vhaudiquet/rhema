//! rhema-memory — cross-session agent memory
//! (specs/rhema-memory.md).
//!
//! Durable memory the model *pulls* through tools, never context the
//! engine pushes: a [`MemoryRecord`] is one thing worth remembering, held
//! in a [`Scope`] (global, or one workspace), written and read only
//! through the [`MemoryStore`] contract and the tools on
//! [`MemoryResolver`]. The contract promises ranked-best-first search and
//! nothing about the ranking method, so a semantic ranker can replace the
//! keyword one without a signature change — the framework spec §9
//! requirement, restated as the trait's shape.
//!
//! Records carry their temporal facts: a validity window, a decay (how
//! soon to doubt them), provenance, a subject overlap key, a status, and
//! a verify hint. The engine computes what is stale, what is due for
//! review, and what may conflict with what; it never decides that a fact
//! became false. Time never kills a record — only supersession or
//! retraction does, and both keep the body, so the history of what was
//! believed stays readable. Search default-rules surface only active
//! records inside their window, and every rendered record carries its
//! staleness to the consumer.
//!
//! The store trait is sync, like `rhema-rsd`'s `Store`: operations are
//! short local reads and writes, and the async seam is the tool resolver
//! above the store. There is no dispatch machinery here — no leases, no
//! inbox, no revisions; a memory operation is short, serialized by the
//! store, and durable on return. Review is not a daemon: a caller asks
//! what is due and does the work at a cadence of its own choosing.
//!
//! Layout:
//! - [`record`] — `MemoryRecord`, `Scope`, `MemoryDraft`, the temporal
//!   types (`Decay`, `Source`, `Status`, `VerifyHint`), tags, ids, UTC
//!   rendering
//! - [`store`] — the `MemoryStore` contract and `MemoryError`
//! - [`search`] — `MemoryQuery` (with `as_of` replay) and the v1
//!   keyword+recency ranker under the validity rules
//! - [`conflict`] — subject-overlap detection for the write path
//! - [`review`] — the due-for-review sweep
//! - [`volatile`] — the in-process `VolatileStore`
//! - [`file`] — the markdown-file `FileMemoryStore`, the durable default
//! - [`tools`] — the memory tools and `MemoryResolver`
//! - [`store_contract`] — the replayable behavioral suite (`store-contract` feature)

#![forbid(unsafe_code)]

pub mod conflict;
pub mod file;
pub mod record;
pub mod review;
pub mod search;
pub mod store;
// The behavioral suite is test machinery: in every test build, and in a
// normal build only when the `store-contract` feature asks for it.
#[cfg(any(test, feature = "store-contract"))]
pub mod store_contract;
pub mod tools;
pub mod volatile;

pub use conflict::{Conflict, TAG_OVERLAP_THRESHOLD, find_conflicts};
pub use file::FileMemoryStore;
pub use record::{
    BODY_LIMIT, DAY_MILLIS, Decay, MemoryDraft, MemoryRecord, Scope, Source, Status, VerifyHint,
    decay_from_tags, normalize_subject,
};
pub use review::{ReviewItem, due_for_review};
pub use search::{MemoryQuery, SEARCH_LIMIT};
pub use store::{MemoryError, MemoryStore};
pub use tools::{
    MEMORY_DUE, MEMORY_FORGET, MEMORY_RETRACT, MEMORY_SAVE, MEMORY_SEARCH, MEMORY_VERIFY,
    MemoryResolver, MemoryToolError,
};
pub use volatile::VolatileStore;
