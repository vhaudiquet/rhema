//! The memory contract: four operations, any backend.
//!
//! Sync, like `rhema-rsd`'s `Store`: the operations are short local reads
//! and writes, and the async seam is the tool resolver above the store.
//! The ranking inside [`MemoryStore::search`] is deliberately
//! unspecified — every caller treats results as ranked-best-first and
//! nothing else, which is what lets a semantic ranker replace the keyword
//! one without a signature change (specs/rhema-memory.md §3).

use std::path::PathBuf;

use crate::record::{MemoryDraft, MemoryRecord};
use crate::search::MemoryQuery;

/// The memory contract: put, get, search, forget. Implementations are
/// `Send + Sync` and serialize their own mutations; a `put` that returned
/// `Ok` is durable.
pub trait MemoryStore: Send + Sync {
    /// Insert, or replace the record with the draft's id (`created_at`
    /// preserved, scope re-aimed). An update naming an absent record is
    /// [`MemoryError::UnknownId`].
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, MemoryError>;

    /// The record with this id, if any.
    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError>;

    /// Best-first under the store's ranking; an empty query means recent.
    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError>;

    /// Remove by id; `Ok(false)` when absent — not an error.
    fn forget(&self, id: &str) -> Result<bool, MemoryError>;
}

/// Why a memory operation failed. Tool callers turn every variant into an
/// in-band result; nothing aborts a turn.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// A draft's body was empty or whitespace.
    #[error("memory body is empty")]
    EmptyBody,
    /// A draft's body exceeded [`crate::BODY_LIMIT`].
    #[error("memory body of {len} bytes exceeds the {limit}-byte limit")]
    BodyTooLong {
        /// The offending body's size.
        len: usize,
        /// The cap that rejected it.
        limit: usize,
    },
    /// An id outside the store's shape — non-empty, at most 64 chars,
    /// `[0-9A-Za-z-]` — was rejected before any lookup.
    #[error("invalid id {id:?}")]
    InvalidId {
        /// The rejected id.
        id: String,
    },
    /// A `put` update named an id the store does not have.
    #[error("no record with id {id:?}")]
    UnknownId {
        /// The absent id.
        id: String,
    },
    /// The backend failed on its filesystem.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A record file exists but does not parse. `search` skips these; a
    /// direct `get` or `put` update reports it, and `forget` still
    /// removes it.
    #[error("record file {path:?} is corrupt: {reason}")]
    Corrupt {
        /// The file that would not parse.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
}
