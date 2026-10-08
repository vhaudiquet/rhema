//! The `Source` contract (spec §5): deterministic detection into the
//! dispatcher.
//!
//! A source owns one input stream — a forge watch set, a webhook
//! receiver — and never invokes the model: its `detect` pass is the
//! gate-before-LLM check, and cadence belongs to the daemon's loop, not
//! to the source or to any timer inside this crate.

use std::sync::Arc;

use rhema_rsd::{DispatchError, KeyFn};

/// Why a detection pass failed: the backend call, or the dispatcher seam
/// behind the ingest.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The source's own backend failed.
    #[error("source detection failed: {0}")]
    Backend(String),
    /// The dispatcher rejected or could not persist an event.
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

/// One detectable input stream. The dispatcher must know this source's
/// [`Source::name`] — register its [`Source::key_fn`] before the first
/// `detect`.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// The name events from this source are ingested under (e.g.
    /// `"github"`); matches the dispatcher's registered source.
    fn name(&self) -> &str;

    /// The key function: event to `ResourceKey`. Reported so a consumer
    /// can register sources without reaching into the implementation.
    fn key_fn(&self) -> Arc<KeyFn>;

    /// One detection pass: poll the outside world, ingest what changed.
    /// Deterministic, model-free; returns how many events were fed
    /// (replayed dedup ids included — the dispatcher dedups).
    async fn detect(&self) -> Result<usize, SourceError>;
}
