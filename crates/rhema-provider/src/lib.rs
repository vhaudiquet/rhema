//! `rhema-provider` — the shared base of the rhema provider adapters.
//!
//! Every `rhema-provider-*` crate implements rhema-core's `ModelProvider`
//! seam against one backend API; this crate holds only what they all
//! share: the [`ModelCard`] their catalog listings normalize into, the
//! [`Pricing`](crate::pricing::Pricing) record attached to priced rows,
//! and the [`SseDecoder`](crate::stream::SseDecoder) their SSE streams
//! decode through. What varies per backend stays in the adapter —
//! including which of its listed models count as text chat models, so a
//! `list_models` call only ever hands out rows the coding agent can drive.
//!
//! Layout:
//! - [`card`] — the normalized catalog row
//! - [`pricing`] — the per-MTok price record
//! - [`stream`] — the SSE byte-stream decoder

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod card;
pub mod pricing;
pub mod stream;

pub use card::ModelCard;
pub use pricing::Pricing;
pub use stream::SseDecoder;
