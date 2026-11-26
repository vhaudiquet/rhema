//! The OpenCode Console and Go adapter for the
//! [`DecisionProvider`](rhema_decision::DecisionProvider) seam.
//!
//! OpenCode serves decision models behind one System One route,
//! `POST {root}/systemone/v1/systemone`: the key in an
//! `Authorization: Bearer` header, the request the System One body with
//! the model as a name (`jev-1.14` by default, `gemini-decision-1`,
//! `gpt-6-luna-decision`). Two roots matter:
//! - [`DEFAULT_BASE_URL`]: pay-as-you-go from the Console balance.
//! - [`GO_BASE_URL`]: the Go, Go Plus, and Go Teams subscription.
//!
//! The Gateway answers Jev's wire, so the adapter is Jev's thirty lines
//! with another root and no conversation header: decision requests do
//! not need `x-opencode-session`, so a RAG script or a memory worker
//! can call them with a Go key (`specs/rhema-decision.md` §4).
//!
//! Layout: the [`wire`] types and the pure shaping (always compiled),
//! and the [`OpenCodeProvider`] over reqwest (feature `native`).

#![forbid(unsafe_code)]

pub mod wire;

#[cfg(feature = "native")]
pub mod provider;

#[cfg(feature = "native")]
pub use provider::{DEFAULT_BASE_URL, GO_BASE_URL, OpenCodeProvider, SYSTEMONE_PATH, USER_AGENT};

pub use wire::{DEFAULT_MODEL, request_body, response_from_json};
