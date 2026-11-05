//! `rhema-forge-integration` — the forge integration: one normalized
//! event vocabulary and one client contract for GitHub, Gitea, and
//! GitLab, and the ingest path that turns forge activity into RSD
//! dispatch.
//!
//! An integration, not an agent: no driver, no model, no loop lives
//! here. A consumer composes the crate with its own agent tier — the
//! [`crate::Watcher`] polls a [`crate::Forge`] for activity and feeds
//! the events to a [`rhema_rsd::Dispatcher`](rhema_rsd) (the forge's
//! stable dedup id becomes the event id, and the resource key is spec
//! §8's `host:kind:owner/repo#number` shape, so re-polling unchanged
//! history dedups away and a changed row re-dispatches), and a dispatched
//! thread is answered through the same [`crate::Forge`] capabilities,
//! with [`crate::decode`] recovering the event behind an inbox item.
//! Polling cadence, handler policy, and the loop itself are the
//! consumer's — the claw daemon today, any other agent consumer of the
//! framework tomorrow; the dispatch guarantees are the dispatcher's.
//!
//! ```
//! use rhema_forge_integration::{Forge, GitHubForge, RepoRef};
//!
//! # async fn demo() -> Result<(), rhema_forge_integration::ForgeError> {
//! let forge = GitHubForge::new(std::env::var("GITHUB_TOKEN").unwrap());
//! // First poll: everything. Later polls: pass `page.cursor` back in.
//! let page = forge.poll(&RepoRef::new("acme", "widgets"), None).await?;
//! for event in &page.events {
//!     println!("{:?} on {}#{}", event.kind, event.repo.path(), event.number);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Layout:
//! - [`event`] — the normalized vocabulary: `ForgeEvent`, `EventKind`,
//!   `RepoRef`, `Snapshot`, `Timestamp`
//! - [`error`] — the crate's single forge error type
//! - [`forge`] — the `Forge` client contract and `ForgePage`
//! - [`github`] — the GitHub adapter (api.github.com)
//! - [`gitea`] — the Gitea adapter (self-hosted, `/api/v1`)
//! - [`gitlab`] — the GitLab adapter (gitlab.com or self-hosted, `/api/v4`)
//! - [`watcher`] — the dispatcher seam: polls a forge into an RSD
//!   `Dispatcher` (keys, dedup, cursors) and decodes delivered inbox
//!   items back into forge events
//! - [`triage`] — what an account notification earns: the default
//!   classification of the notification vocabulary into agent
//!   dispatches, surfaces, and drops

#![forbid(unsafe_code)]

pub mod error;
pub mod event;
pub mod forge;
pub mod gitea;
pub mod kind;
pub use kind::{ForgeKind, resolve_token};
pub mod github;
pub mod gitlab;
pub mod triage;
pub mod watcher;

// Re-exported so implementors of the async seam can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use error::ForgeError;
pub use event::{
    EventKind, ForgeEvent, ForgeThreadKind, Notification, NotificationPage, NotificationReason,
    RepoRef, Snapshot, ThreadState, Timestamp,
};
pub use forge::{Forge, ForgePage};
pub use gitea::GiteaForge;
pub use github::GitHubForge;
pub use gitlab::GitLabForge;
pub use triage::{Role, Triage};
pub use watcher::{
    WatchError, Watcher, decode, forge_key_fn, parse_key, register_forge, resource_key,
};
