//! The watch step: poll a forge feed, hand the events to the RSD
//! dispatcher.
//!
//! The [`Watcher`] owns one cursor per repo (the newest event time fed to
//! the dispatcher) and turns forge events into
//! [`rhema_rsd::Event`]s: the forge's stable dedup id stays the event id,
//! the serialized [`ForgeEvent`](crate::event::ForgeEvent) is the payload,
//! and the resource key is the spec §8 shape —
//! `{source}:{owner}/{name}#{number}` (see [`resource_key`]). Re-polling
//! unchanged history therefore dedups away in the dispatcher, and a
//! changed row re-dispatches.
//!
//! The watcher is loop-free: one `poll` per call. Cadence belongs to the
//! embedder's loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rhema_rsd::{Clock, Dispatcher, Event, KeyFn, ResourceKey, SystemClock};

use crate::error::ForgeError;
use crate::event::{RepoRef, Timestamp};
use crate::forge::Forge;

/// Why a watch step failed: the forge call, or the dispatcher seam behind
/// it.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The poll itself failed.
    #[error(transparent)]
    Forge(#[from] ForgeError),
    /// The dispatcher rejected or could not persist an event.
    #[error(transparent)]
    Dispatch(#[from] rhema_rsd::DispatchError),
}

/// The resource key of a thread on `source`: `{source}:{path}#{number}` —
/// spec §8's `github:acme/widgets#142` shape.
pub fn resource_key(source: &str, repo: &RepoRef, number: u64) -> ResourceKey {
    ResourceKey::from(format!("{source}:{}#{number}", repo.path()))
}

/// Recovers the thread coordinates from [`resource_key`]'s shape. The
/// inverse of the key builder, so a dispatch never needs the original
/// event to reach the forge.
pub fn parse_key(key: &ResourceKey) -> Option<(RepoRef, u64)> {
    let (head, number) = key.as_str().rsplit_once('#')?;
    let path = head.split_once(':')?.1;
    let (owner, name) = path.rsplit_once('/')?;
    Some((RepoRef::new(owner, name), number.parse().ok()?))
}

/// The payload a watcher builds for every event: just the coordinates the
/// canonical key function reads, before the full [`ForgeEvent`].
#[derive(serde::Deserialize)]
struct KeyParts {
    repo: RepoRef,
    number: u64,
}

/// Registers `source` on `dispatcher` with the canonical key function —
/// the one [`resource_key`] implements. Do this once before the first
/// ingest.
pub fn register_forge(dispatcher: &Dispatcher, source: &str) {
    let prefix = format!("{source}:");
    dispatcher.register_source(
        source.to_owned(),
        Arc::new(move |event: &Event| {
            match serde_json::from_value::<KeyParts>(event.payload.clone()) {
                Ok(parts) => {
                    ResourceKey::from(format!("{prefix}{}#{}", parts.repo.path(), parts.number))
                }
                // Infallible seam: a payload the watcher itself did not
                // build still gets a unique key, so it can neither clobber
                // another thread nor wedge the dispatcher.
                Err(_) => ResourceKey::from(format!("{prefix}unparsable#{}", event.id)),
            }
        }),
    );
}

/// The `KeyFn` [`register_forge`] installs, for embedders wiring the
/// source themselves.
pub fn forge_key_fn(source: &str) -> Arc<KeyFn> {
    let prefix = format!("{source}:");
    Arc::new(
        move |event: &Event| match serde_json::from_value::<KeyParts>(event.payload.clone()) {
            Ok(parts) => {
                ResourceKey::from(format!("{prefix}{}#{}", parts.repo.path(), parts.number))
            }
            Err(_) => ResourceKey::from(format!("{prefix}unparsable#{}", event.id)),
        },
    )
}

/// Polls forge feeds into a [`Dispatcher`], one repo at a time, keeping
/// each repo's cursor.
pub struct Watcher {
    dispatcher: Arc<Dispatcher>,
    source: String,
    cursors: Mutex<HashMap<RepoRef, Timestamp>>,
}

impl Watcher {
    /// A watcher feeding `dispatcher` as `source` (e.g. `"github"`).
    /// [`register_forge`] must have installed the source's key function.
    pub fn new(dispatcher: Arc<Dispatcher>, source: impl Into<String>) -> Self {
        Self {
            dispatcher,
            source: source.into(),
            cursors: Mutex::new(HashMap::new()),
        }
    }

    /// The cursor `repo` polls from next: `None` before the first poll
    /// (everything the forge returns is fed), then the newest event time
    /// seen.
    pub fn cursor(&self, repo: &RepoRef) -> Option<Timestamp> {
        self.cursors
            .lock()
            .expect("cursor map lock")
            .get(repo)
            .cloned()
    }

    /// One poll of `repo`: the page's events are ingested in feed order
    /// and the cursor advances to the page's. Returns how many events were
    /// fed to the dispatcher — replayed ids included, they dedup there.
    pub async fn poll(&self, forge: &dyn Forge, repo: &RepoRef) -> Result<usize, WatchError> {
        let since = self.cursor(repo);
        let page = forge.poll(repo, since.as_ref()).await?;
        let now = SystemClock.now_millis();
        let mut fed = 0;
        for event in &page.events {
            let payload = serde_json::to_value(event)
                .map_err(|err| ForgeError::Parse(format!("event does not serialize: {err}")))?;
            self.dispatcher.ingest(Event {
                id: event.id.clone(),
                source: self.source.clone(),
                payload,
                received_at: now,
            })?;
            fed += 1;
        }
        if let Some(cursor) = page.cursor {
            self.cursors
                .lock()
                .expect("cursor map lock")
                .insert(repo.clone(), cursor);
        }
        Ok(fed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_shape_round_trips_through_parse() {
        let repo = RepoRef::new("acme", "widgets");
        let key = resource_key("github", &repo, 142);
        assert_eq!(key.as_str(), "github:acme/widgets#142");
        assert_eq!(parse_key(&key), Some((repo, 142)));
    }

    #[test]
    fn a_gitlab_subgroup_path_survives_the_round_trip() {
        let repo = RepoRef::new("group/subgroup", "widgets");
        let key = resource_key("gitlab", &repo, 7);
        assert_eq!(parse_key(&key), Some((repo, 7)));
    }

    #[test]
    fn a_key_without_a_number_does_not_parse() {
        assert_eq!(parse_key(&ResourceKey::from("github:acme/widgets")), None);
    }
}
