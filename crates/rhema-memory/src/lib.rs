//! rhema-memory — cross-session agent memory
//! (specs/rhema-memory.md).
//!
//! Durable memory the model *pulls* through tools, never context the
//! engine pushes: a [`MemoryRecord`] is one thing worth remembering, held
//! in a [`Scope`] (global, or one workspace), written and read only
//! through the [`MemoryStore`] contract and the three tools on
//! [`MemoryResolver`]. The contract promises ranked-best-first search and
//! nothing about the ranking method, so a semantic ranker can replace the
//! keyword one without a signature change — the framework spec §9
//! requirement, restated as the trait's shape.
//!
//! The store trait is sync, like `rhema-rsd`'s `Store`: operations are
//! short local reads and writes, and the async seam is the tool resolver
//! above the store. There is no dispatch machinery here — no leases, no
//! inbox, no revisions; a memory operation is short, serialized by the
//! store, and durable on return.
//!
//! Layout:
//! - [`record`] — `MemoryRecord`, `Scope`, `MemoryDraft`, tags, ids, UTC
//!   rendering
//! - [`store`] — the `MemoryStore` contract and `MemoryError`
//! - [`search`] — `MemoryQuery` and the v1 keyword+recency ranker
//! - [`volatile`] — the in-process `VolatileStore`
//! - [`file`] — the markdown-file `FileMemoryStore`, the durable default
//! - [`store_contract`] — the replayable behavioral suite

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod file;
pub mod record;
pub mod search;
pub mod store;
pub mod store_contract;
pub mod volatile;

pub use file::FileMemoryStore;
pub use record::{BODY_LIMIT, MemoryDraft, MemoryRecord, Scope};
pub use search::{MemoryQuery, SEARCH_LIMIT};
pub use store::{MemoryError, MemoryStore};
pub use volatile::VolatileStore;
