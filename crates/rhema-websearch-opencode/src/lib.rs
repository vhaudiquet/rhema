//! The OpenCode hosted web search adapter for the
//! [`WebSearchProvider`](rhema_websearch::WebSearchProvider) seam.
//!
//! A workspace with hosted web search enabled serves it through
//! Console, `POST https://opencode.ai/console/api/websearch`: the key
//! in an `Authorization: Bearer` header, a `{"query", "providerID":
//! "opencode"}` body, and `{"providerID", "results"}` back. Each
//! successful search bills the workspace $0.01, and a failed or
//! rate-limited one bills nothing.
//!
//! A Go key is a Console key, so a Go subscription reads through the
//! same route once the workspace's owner enables hosted search. The
//! route has no count or news field: it returns up to eight
//! general-web results, and the adapter trims to the caller's count.
//! `--news` cannot narrow a route with no news index.
//!
//! Layout: the [`wire`] types and the pure mapping (always compiled),
//! and the [`OpenCodeProvider`] over reqwest (feature `native`).

#![forbid(unsafe_code)]

pub mod wire;

#[cfg(feature = "native")]
pub mod provider;

#[cfg(feature = "native")]
pub use provider::{DEFAULT_BASE_URL, OpenCodeProvider, USER_AGENT, WEBSEARCH_PATH};

pub use wire::{
    OpenCodeSearchRequest, OpenCodeSearchResponse, OpenCodeSearchResult, OpenCodeSearchTime,
    PROVIDER_ID, results_from_response,
};
