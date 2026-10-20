//! The bounded view over an unbounded session.
//!
//! Every step sends `session.history.clone()` (SPEC.md §4): cost grows
//! with the session, and the context window ends the session that
//! outgrows it. This crate splits what was fused. The **log** — the
//! caller's `Session` — stays complete; the **view** — what a request
//! carries — is a projection built per request under a byte budget.
//! Old messages are represented by short lines, nothing is deleted
//! anywhere, and `zoom` is the model's way back to any original.
//!
//! - [`tree`] — the slot algebra: ranges `(id, n)`, halves, the old
//!   prefix's doubling cover
//! - [`summary`] — the extractive line text: heads and counts, no
//!   model calls
//! - [`view`] — the request view: the provider decorator, the mirror,
//!   the budget
//! - [`zoom`] — the `zoom` tool
//!
//! The design is `specs/rhema-transcript.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod summary;
pub mod tree;
pub mod view;
pub mod zoom;

pub use view::{Transcript, TranscriptSettings, TranscriptView};
pub use zoom::{ZOOM_MAX_BYTES, Zoom};

use std::sync::{Arc, Mutex};

/// Wires a transcript over `inner`: the view decorator plus the `zoom`
/// resolver, sharing one mirror. A consumer that builds its own
/// [`rhema_core::Agent`] sends the wrapped provider through to the
/// builder and overlays the resolver over its tool chain; the coding
/// agent does both inside [`rhema_coding_agent::CodingAgent::new`]
/// when its config enables the transcript.
///
/// The mirror starts empty; the first request fills it.
pub fn attach(
    inner: Arc<dyn rhema_core::ModelProvider>,
    settings: TranscriptSettings,
) -> (Arc<dyn rhema_core::ModelProvider>, Zoom) {
    let transcript = Arc::new(Mutex::new(Transcript::new(settings)));
    let view = TranscriptView::new(inner, transcript.clone());
    let zoom = Zoom::new(transcript);
    (Arc::new(view), zoom)
}
