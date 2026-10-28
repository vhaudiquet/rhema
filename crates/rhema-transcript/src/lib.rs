//! The bounded view over an unbounded session.
//!
//! Every step sends `session.history.clone()` (SPEC.md §4): cost grows
//! with the session, and the context window ends the session that
//! outgrows it. This crate splits what was fused. The **log** — the
//! caller's `Session` — stays complete; the **view** — what a request
//! carries — is a projection built per request under a byte budget.
//! Old messages are represented by the stepped compaction block,
//! nothing is deleted anywhere, and `zoom` is the model's way back to
//! any original.
//!
//! - [`tree`] — the slot algebra: ranges `(id, n)`, halves
//! - [`summary`] — the extractive line text: heads and counts, no
//!   model calls
//! - [`compact`] — the stepped compaction engine: the append-only
//!   block, the summarizer seam, the model worker
//! - [`view`] — the request view: the provider decorator, the mirror,
//!   the budget, the benchmark stats
//! - [`zoom`] — the `zoom` tool
//!
//! The design is `specs/rhema-transcript.md`.

#![forbid(unsafe_code)]

pub mod compact;
pub mod summary;
pub mod tree;
pub mod view;
pub mod zoom;

pub use compact::{ModelSummarizer, PendingSummary, Section, SectionOrigin, Summarizer};
pub use view::{
    LogContent, Transcript, TranscriptSettings, TranscriptStats, TranscriptView, UsageBreakdown,
    WorkerOutcome,
};
pub use zoom::{ZOOM_MAX_BYTES, Zoom};

use std::sync::{Arc, Mutex};

/// Wires a transcript over `inner`: the view decorator plus the `zoom`
/// resolver, sharing one mirror. A consumer that builds its own
/// [`rhema_core::Agent`] sends the wrapped provider through to the
/// builder and overlays the resolver over its tool chain; the coding
/// agent does both in its constructor when its config enables the
/// transcript.
///
/// The mirror starts empty; the first request fills it. Sections keep
/// their extractive stand-ins — pass a summarizer to
/// [`attach_with_summarizer`] for the model fill.
pub fn attach(
    inner: Arc<dyn rhema_core::ModelProvider>,
    settings: TranscriptSettings,
) -> (Arc<dyn rhema_core::ModelProvider>, Zoom) {
    let (provider, zoom, _mirror) = attach_with_summarizer(inner, settings, None);
    (provider, zoom)
}

/// [`attach`] with the compaction's model fill: `summarizer` rewrites
/// each new section's extractive stand-in into a dense block at flush
/// time. `None` is the extractive-only compaction. Returns the mirror
/// too, for a driver that reports the benchmark stats.
pub fn attach_with_summarizer(
    inner: Arc<dyn rhema_core::ModelProvider>,
    settings: TranscriptSettings,
    summarizer: Option<Arc<dyn Summarizer>>,
) -> (
    Arc<dyn rhema_core::ModelProvider>,
    Zoom,
    Arc<Mutex<Transcript>>,
) {
    let transcript = Arc::new(Mutex::new(Transcript::new(settings)));
    let view = match summarizer {
        Some(summarizer) => TranscriptView::with_summarizer(inner, transcript.clone(), summarizer),
        None => TranscriptView::new(inner, transcript.clone()),
    };
    let zoom = Zoom::new(transcript.clone());
    (Arc::new(view), zoom, transcript)
}
