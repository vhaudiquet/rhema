//! `rhema-provider` — the shared base of the rhema provider adapters.
//!
//! Every `rhema-provider-*` crate implements rhema-core's `ModelProvider`
//! seam against one backend API; this crate holds only what they all
//! share: the [`ModelCard`] their catalog listings normalize into, the
//! [`Pricing`](crate::pricing::Pricing) record attached to priced rows,
//! the [`Allowance`](crate::allowance::Allowance) their account reports,
//! and the [`SseDecoder`](crate::stream::SseDecoder) their SSE streams
//! decode through. What varies per backend stays in the adapter —
//! including which of its listed models count as text chat models, so a
//! `list_models` call only ever hands out rows the coding agent can drive.
//! The one exception is the [`login`](crate::login) seam: OAuth-style
//! handshakes repeat across backends, so an adapter declares its flow
//! there and any frontend renders it without knowing the protocol.
//!
//! Layout:
//! - [`card`] — the normalized catalog row
//! - [`pricing`] — the per-MTok price record
//! - [`allowance`] — the subscription allowance a backend reports
//! - [`stream`] — the SSE byte-stream decoder
//! - [`login`] — the interactive login flow seam
//! - [`retry`] — the transient-failure retry every turn provider rides
//! - [`transport`] — the HTTP seam under the adapters: one request in,
//!   a status and a `Send` byte stream out

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod allowance;
pub mod card;
pub mod login;
pub mod pricing;
pub mod retry;
pub mod stream;
pub mod transport;

pub use allowance::Allowance;
pub use card::ModelCard;
pub use login::{InteractiveLogin, LoginInstruction, LoginSink, LoginUpdate};
pub use pricing::Pricing;
pub use retry::{Retrying, Sleeper, TokioSleeper};
pub use stream::SseDecoder;
pub use transport::{ByteStream, HttpRequest, HttpResponse, Transport, TransportError};
