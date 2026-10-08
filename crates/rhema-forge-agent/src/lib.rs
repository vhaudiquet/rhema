//! `rhema-forge-agent` — the forge-watching bot: one normalized event
//! vocabulary and one client contract for GitHub, Gitea, and GitLab, and
//! the dispatch glue that turns forge activity into RSD handler runs.
//!
//! The [`crate::Watcher`] polls a [`crate::Forge`] for activity and feeds
//! the events to a [`rhema_rsd::Dispatcher`](rhema_rsd) — the forge's
//! stable dedup id becomes the event id, and the resource key is spec §8's
//! `host:owner/repo#number` shape, so re-polling unchanged history dedups
//! away and a changed row re-dispatches. The [`crate::ForgeBot`] closes
//! the loop: every admitted key acquires the lease, fetches the thread's
//! snapshot, and runs the embedder's [`crate::Handler`] with a live inbox,
//! a resumable session, and the forge capabilities — one handler per
//! thread, ever, surviving restarts. Polling cadence and handler policy
//! are the embedder's; the dispatch guarantees are the dispatcher's.
//!
//! ```
//! use rhema_forge_agent::{Forge, GitHubForge, RepoRef};
//!
//! # async fn demo() -> Result<(), rhema_forge_agent::ForgeError> {
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
//! - [`watcher`] — polls a forge into an RSD `Dispatcher` (keys, dedup,
//!   cursors)
//! - [`bot`] — the `ForgeBot`: admitted keys become `Handler` runs with a
//!   live inbox and a resumable session

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod bot;
pub mod error;
pub mod event;
pub mod forge;
pub mod gitea;
pub mod github;
pub mod gitlab;
pub mod watcher;

// Re-exported so implementors of the async seam can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use bot::{ForgeBot, Handler, ThreadContext, decode};
pub use error::ForgeError;
pub use event::{EventKind, ForgeEvent, RepoRef, Snapshot, ThreadState, Timestamp};
pub use forge::{Forge, ForgePage};
pub use gitea::GiteaForge;
pub use github::GitHubForge;
pub use gitlab::GitLabForge;
pub use watcher::{WatchError, Watcher, parse_key, register_forge, resource_key};
