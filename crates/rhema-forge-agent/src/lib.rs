//! `rhema-forge-agent` — forge clients for the forge-watching bot: one
//! normalized event vocabulary and one client contract, backed by GitHub,
//! Gitea, and GitLab.
//!
//! The bot (a later layer in this crate) polls a forge for activity and
//! hands the events to the RSD dispatcher (see
//! specs/resource-scoped-dispatch.md). Every [`ForgeEvent`] carries the
//! resource coordinates the dispatcher keys on ([`RepoRef`] plus the
//! thread number, the `host:owner/repo#number` shape of RSD §8) and a
//! stable dedup id derived from the forge's own item id and the row's
//! updated time, so re-polling unchanged history dedups away and a
//! changed row re-dispatches. Each [`Forge`] implementation is a
//! standalone REST client: a base URL, a token, and the three calls the
//! bot needs — [`Forge::poll`], [`Forge::comment`], [`Forge::snapshot`].
//! Polling cadence, scheduling, and handler policy are the watcher's
//! concerns, not this layer's.
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
//! - [`error`] — the crate's single error type
//! - [`forge`] — the `Forge` client contract and `ForgePage`
//! - [`github`] — the GitHub adapter (api.github.com)
//! - [`gitea`] — the Gitea adapter (self-hosted, `/api/v1`)
//! - [`gitlab`] — the GitLab adapter (gitlab.com or self-hosted, `/api/v4`)

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod event;
pub mod forge;
pub mod gitea;
pub mod github;
pub mod gitlab;

// Re-exported so implementors of the async seam can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use error::ForgeError;
pub use event::{EventKind, ForgeEvent, RepoRef, Snapshot, ThreadState, Timestamp};
pub use forge::{Forge, ForgePage};
pub use gitea::GiteaForge;
pub use github::GitHubForge;
pub use gitlab::GitLabForge;
