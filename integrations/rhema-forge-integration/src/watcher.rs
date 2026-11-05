//! The dispatcher seam: poll a forge feed into the RSD dispatcher, and
//! decode a delivered inbox item back into the forge event it carried.
//!
//! The [`Watcher`] owns one cursor per repo (the newest event time fed to
//! the dispatcher) and turns forge events into
//! [`rhema_rsd::Event`]s: the forge's stable dedup id stays the event id,
//! the serialized [`ForgeEvent`](crate::event::ForgeEvent) is the payload,
//! and the resource key is the spec §8 shape —
//! `{source}:{kind}:{owner}/{name}#{number}` (see [`resource_key`]). Re-polling
//! unchanged history therefore dedups away in the dispatcher, and a
//! changed row re-dispatches.
//!
//! The watcher is loop-free: one `poll` per call. Cadence belongs to the
//! embedder's loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rhema_rsd::{Clock, Dispatcher, Event, InboxItem, KeyFn, ResourceKey, SystemClock};

use crate::error::ForgeError;
use crate::event::{ForgeEvent, ForgeThreadKind, RepoRef, Timestamp};
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

/// The resource key of a thread on `source`:
/// `{source}:{kind}:{owner}/{name}#{number}` — spec §8's shape with the
/// thread kind spelled between source and path, as `github:issue:acme/
/// widgets#142` or `gitlab:mr:acme/widgets#7`. The kind is part of the
/// key because GitLab numbers issues and merge requests from separate
/// sequences: without it, issue #7 and merge request #7 would share one
/// key, one dedup lane, and one agent session.
pub fn resource_key(
    source: &str,
    repo: &RepoRef,
    kind: ForgeThreadKind,
    number: u64,
) -> ResourceKey {
    ResourceKey::from(format!(
        "{source}:{}:{}#{number}",
        key_segment(source, kind),
        repo.path()
    ))
}

/// The key segment `source` spells `kind` with: GitLab says `mr` where
/// the rest say `pull`, so a key reads like the forge's own dedup ids
/// (`github:pull:…`, `gitlab:mr:…`) it rides beside.
fn key_segment(source: &str, kind: ForgeThreadKind) -> &'static str {
    match (kind, source == "gitlab") {
        (ForgeThreadKind::Issue, _) => "issue",
        (ForgeThreadKind::PullRequest, true) => "mr",
        (ForgeThreadKind::PullRequest, false) => "pull",
    }
}

/// The kind a key segment names: `issue`, or the pull variant in either
/// spelling (`pull` on GitHub and Gitea, `mr` on GitLab). `None` for
/// anything else — an old-format key has no segment and parses as none.
fn segment_kind(segment: &str) -> Option<ForgeThreadKind> {
    match segment {
        "issue" => Some(ForgeThreadKind::Issue),
        "pull" | "mr" => Some(ForgeThreadKind::PullRequest),
        _ => None,
    }
}

/// Recovers the thread coordinates from [`resource_key`]'s shape. The
/// inverse of the key builder, so a dispatch never needs the original
/// event to reach the forge. Pre-kind keys (`gitlab:acme/w#7`) parse as
/// `None` rather than guess a sequence.
pub fn parse_key(key: &ResourceKey) -> Option<(RepoRef, ForgeThreadKind, u64)> {
    let (head, number) = key.as_str().rsplit_once('#')?;
    let (_, rest) = head.split_once(':')?;
    let (segment, path) = rest.split_once(':')?;
    let kind = segment_kind(segment)?;
    let (owner, name) = path.rsplit_once('/')?;
    Some((RepoRef::new(owner, name), kind, number.parse().ok()?))
}

/// The thread kind a payload's `is_pr` field names. Both payload builders
/// this crate ships (the watcher's serialized [`ForgeEvent`] and the claw's
/// triaged notification) carry `is_pr` at the top level, so the key
/// function reads the wire field instead of redefining the payload.
fn thread_kind(is_pr: bool) -> ForgeThreadKind {
    if is_pr {
        ForgeThreadKind::PullRequest
    } else {
        ForgeThreadKind::Issue
    }
}

/// The payload a watcher builds for every event: just the coordinates the
/// canonical key function reads, before the full [`ForgeEvent`].
#[derive(serde::Deserialize)]
struct KeyParts {
    repo: RepoRef,
    number: u64,
    /// The thread kind, in its wire spelling. No default: a payload
    /// without it cannot name a GitLab sequence, and guessing `issue`
    /// would rebuild the ambiguity this key shape exists to prevent —
    /// such a payload lands on the unparsable key instead.
    is_pr: bool,
}

/// Registers `source` on `dispatcher` with the canonical key function —
/// the one [`resource_key`] implements. Do this once before the first
/// ingest.
pub fn register_forge(dispatcher: &Dispatcher, source: &str) {
    let source = source.to_owned();
    dispatcher.register_source(
        source.clone(),
        Arc::new(move |event: &Event| {
            match serde_json::from_value::<KeyParts>(event.payload.clone()) {
                Ok(parts) => {
                    resource_key(&source, &parts.repo, thread_kind(parts.is_pr), parts.number)
                }
                // Infallible seam: a payload the watcher itself did not
                // build still gets a unique key, so it can neither clobber
                // another thread nor wedge the dispatcher.
                Err(_) => ResourceKey::from(format!("{source}:unparsable#{}", event.id)),
            }
        }),
    );
}

/// The `KeyFn` [`register_forge`] installs, for embedders wiring the
/// source themselves.
pub fn forge_key_fn(source: &str) -> Arc<KeyFn> {
    let source = source.to_owned();
    Arc::new(
        move |event: &Event| match serde_json::from_value::<KeyParts>(event.payload.clone()) {
            Ok(parts) => resource_key(&source, &parts.repo, thread_kind(parts.is_pr), parts.number),
            Err(_) => ResourceKey::from(format!("{source}:unparsable#{}", event.id)),
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

/// Decodes a delivered inbox item back into the forge event the watcher
/// fed. A payload this crate did not build is a loud parse error, never a
/// silently skipped event.
pub fn decode(item: &InboxItem) -> Result<ForgeEvent, ForgeError> {
    serde_json::from_value(item.event.payload.clone())
        .map_err(|err| ForgeError::Parse(format!("inbox payload is not a forge event: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_shape_round_trips_through_parse() {
        let repo = RepoRef::new("acme", "widgets");
        let key = resource_key("github", &repo, ForgeThreadKind::Issue, 142);
        assert_eq!(key.as_str(), "github:issue:acme/widgets#142");
        assert_eq!(parse_key(&key), Some((repo, ForgeThreadKind::Issue, 142)));
    }

    #[test]
    fn a_gitlab_subgroup_path_survives_the_round_trip() {
        let repo = RepoRef::new("group/subgroup", "widgets");
        let key = resource_key("gitlab", &repo, ForgeThreadKind::Issue, 7);
        assert_eq!(parse_key(&key), Some((repo, ForgeThreadKind::Issue, 7)));
    }

    #[test]
    fn a_gitlab_issue_and_merge_request_produce_distinct_keys() {
        // GitLab numbers the two sequences independently: iid 7 exists
        // twice, and the keys must not collapse them onto one session.
        let repo = RepoRef::new("acme", "widgets");
        let issue = resource_key("gitlab", &repo, ForgeThreadKind::Issue, 7);
        let mr = resource_key("gitlab", &repo, ForgeThreadKind::PullRequest, 7);
        assert_eq!(issue.as_str(), "gitlab:issue:acme/widgets#7");
        assert_eq!(mr.as_str(), "gitlab:mr:acme/widgets#7");
        assert_ne!(issue, mr);
        assert_eq!(
            parse_key(&mr),
            Some((repo, ForgeThreadKind::PullRequest, 7))
        );
    }

    #[test]
    fn a_github_pull_request_key_spells_the_pull_segment() {
        let repo = RepoRef::new("acme", "widgets");
        let pull = resource_key("github", &repo, ForgeThreadKind::PullRequest, 142);
        assert_eq!(pull.as_str(), "github:pull:acme/widgets#142");
        assert_eq!(
            parse_key(&pull),
            Some((repo, ForgeThreadKind::PullRequest, 142))
        );
    }

    #[test]
    fn a_pre_kind_key_parses_as_none_instead_of_guessing() {
        assert_eq!(parse_key(&ResourceKey::from("gitlab:acme/widgets#7")), None);
    }

    #[test]
    fn the_key_fn_separates_issue_and_pull_payloads() {
        let event = |is_pr: bool, number: u64| Event {
            id: format!("gitlab:thread:{number}"),
            source: "gitlab".to_owned(),
            payload: serde_json::to_value(ForgeEvent {
                id: format!("gitlab:thread:{number}@2026-10-08T00:00:00Z"),
                kind: crate::EventKind::Comment,
                repo: RepoRef::new("acme", "widgets"),
                number,
                is_pr,
                actor: None,
                body: None,
                url: None,
                timestamp: Timestamp::parse("2026-10-08T00:00:00Z").expect("parses"),
                raw: serde_json::json!({}),
            })
            .expect("a forge event serializes"),
            received_at: 0,
        };
        let key_fn = forge_key_fn("gitlab");
        assert_eq!(
            key_fn(&event(false, 7)).as_str(),
            "gitlab:issue:acme/widgets#7"
        );
        assert_eq!(key_fn(&event(true, 7)).as_str(), "gitlab:mr:acme/widgets#7");
        // A payload without the kind cannot name a sequence: the
        // unparsable key keeps it off every thread's lane.
        let key = key_fn(&Event {
            id: "foreign".to_owned(),
            source: "gitlab".to_owned(),
            payload: serde_json::json!({ "repo": {"owner": "acme", "name": "widgets"}, "number": 7 }),
            received_at: 0,
        });
        assert_eq!(key.as_str(), "gitlab:unparsable#foreign");
    }

    #[test]
    fn a_key_without_a_number_does_not_parse() {
        assert_eq!(parse_key(&ResourceKey::from("github:acme/widgets")), None);
    }
}
