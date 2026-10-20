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
//!
//! The design is `specs/rhema-transcript.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod summary;
pub mod tree;
pub mod view;

pub use view::{Transcript, TranscriptSettings, TranscriptView};
