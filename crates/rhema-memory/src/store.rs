//! The memory contract: put, get, search, forget — plus the lifecycle a
//! record can go through without ever being erased (supersede, retract,
//! verify, due for review).
//!
//! Sync, like `rhema-rsd`'s `Store`: the operations are short local reads
//! and writes, and the async seam is the tool resolver above the store.
//! The ranking inside [`MemoryStore::search`] is deliberately
//! unspecified — every caller treats results as ranked-best-first and
//! nothing else, which is what lets a semantic ranker replace the keyword
//! one without a signature change (specs/rhema-memory.md §3).

use crate::record::{MemoryDraft, MemoryRecord};
use crate::review::ReviewItem;
use crate::search::MemoryQuery;
use std::path::PathBuf;

/// The memory contract: put, get, search, forget, and the lifecycle that
/// retires records without erasing them. Implementations are `Send + Sync`
/// and serialize their own mutations; a `put` that returned `Ok` is
/// durable.
pub trait MemoryStore: Send + Sync {
    /// Insert, or replace the record with the draft's id (`created_at`
    /// preserved, scope re-aimed). An update naming an absent record is
    /// [`MemoryError::UnknownId`].
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, MemoryError>;

    /// The record with this id, if any.
    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError>;

    /// Best-first under the store's ranking; an empty query means recent.
    /// Default rules surface only records that were active and inside
    /// their validity window — dead records through
    /// [`MemoryQuery::include_superseded`], any instant through
    /// [`MemoryQuery::as_of`].
    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError>;

    /// Remove by id; `Ok(false)` when absent — not an error. Deletion is
    /// for secrets and junk; facts a model or human no longer believes go
    /// through [`MemoryStore::supersede`] or [`MemoryStore::retract`].
    fn forget(&self, id: &str) -> Result<bool, MemoryError>;

    /// Marks `id` superseded by `by`: the record stays, its body intact,
    /// its status recording what replaced it and when. The caller — never
    /// the engine — declares the relation; an unknown id is
    /// [`MemoryError::UnknownId`].
    fn supersede(&self, id: &str, by: &str) -> Result<MemoryRecord, MemoryError>;

    /// Marks `id` retracted for `why`: killed by a human judgement, kept
    /// forever. An unknown id is [`MemoryError::UnknownId`].
    fn retract(&self, id: &str, why: &str) -> Result<MemoryRecord, MemoryError>;

    /// Records a re-confirmation of `id`: `verified_at` moves to now and
    /// nothing else changes. An unknown id is
    /// [`MemoryError::UnknownId`].
    fn verify(&self, id: &str) -> Result<MemoryRecord, MemoryError>;

    /// The active records due for review at `now` — past their decay
    /// interval or past their horizon — most overdue first. The engine
    /// schedules nothing: a caller chooses the cadence and does the work.
    fn due_for_review(&self, now: u64) -> Result<Vec<ReviewItem>, MemoryError>;
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
    /// A subject key outside the shape — lowercase `a-z0-9/_-`, at most
    /// 128 chars — was rejected before any lookup.
    #[error("invalid subject {subject:?}")]
    InvalidSubject {
        /// The rejected subject.
        subject: String,
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
