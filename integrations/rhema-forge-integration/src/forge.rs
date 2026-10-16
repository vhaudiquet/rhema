//! The forge client contract: one trait, three calls.
//!
//! [`Forge`] is the whole surface a consumer needs: poll a repo's
//! activity against a cursor, post a comment, and fetch a thread
//! snapshot so an answering handler can orient without a second API
//! round-trip. Implementations are cheap to clone and share — the
//! reqwest client inside is reference-counted — and object-safe so a
//! backend can be swapped without generics.
//!
//! Polls are at-least-once. The forges' `since`-style filters match rows
//! at-or-after the cursor, so boundary rows repeat across polls, and a
//! row that changes between a backend's feeds can surface twice under
//! different ids. Both are harmless under the RSD dedup: consumers must
//! treat the event `id` as the only dedup key, never `(repo, number)`.

use async_trait::async_trait;

use crate::error::ForgeError;
use crate::event::{ForgeEvent, NotificationPage, RepoRef, Snapshot, Timestamp};

/// One poll's worth of activity, with the cursor to continue from.
#[derive(Debug, Clone, PartialEq)]
pub struct ForgePage {
    /// The events, oldest first, ties broken by id — a deterministic
    /// order regardless of which feed produced an event.
    pub events: Vec<ForgeEvent>,
    /// The cursor for the next poll: the newest event time seen, never
    /// earlier than the poll's `since`. `None` only when the poll had no
    /// anchor and came back empty — the repo is simply quiet.
    pub cursor: Option<Timestamp>,
}

impl ForgePage {
    /// Folds collected events into a page: sorts them and advances the
    /// cursor past everything seen.
    pub(crate) fn finish(mut events: Vec<ForgeEvent>, since: Option<&Timestamp>) -> Self {
        events.sort_by(|a, b| (&a.timestamp, &a.id).cmp(&(&b.timestamp, &b.id)));
        let mut cursor = since.cloned();
        for timestamp in events.iter().map(|event| &event.timestamp) {
            cursor = Some(match cursor {
                Some(current) if current >= *timestamp => current,
                _ => timestamp.clone(),
            });
        }
        Self { events, cursor }
    }
}

/// A forge backend: the client for one REST dialect.
#[async_trait]
pub trait Forge: Send + Sync {
    /// Recent activity for `repo`, oldest first, all pages collected.
    ///
    /// `since` anchors the poll: `None` lists from the beginning (a first
    /// poll); `Some(cursor)` asks for rows at-or-after the cursor, whose
    /// boundary rows may repeat — their stable ids make the repetition
    /// dedupable downstream, never duplicated work.
    async fn poll(
        &self,
        repo: &RepoRef,
        since: Option<&Timestamp>,
    ) -> Result<ForgePage, ForgeError>;

    /// Posts `body` as a new comment on thread `number`.
    async fn comment(&self, repo: &RepoRef, number: u64, body: &str) -> Result<(), ForgeError>;

    /// The thread's current state, so a handler can orient without a
    /// second API round-trip.
    async fn snapshot(&self, repo: &RepoRef, number: u64) -> Result<Snapshot, ForgeError>;

    /// The account's notification feed: everything addressed to the
    /// account this client authenticates as, across every repository it
    /// can see — the account-scoped counterpart of [`Forge::poll`],
    /// whose feeds are repo-scoped. `since` anchors the poll with the
    /// same at-least-once semantics as `poll`: boundary rows repeat, and
    /// the stable ids make the repetition dedupable downstream.
    async fn notifications(
        &self,
        since: Option<&Timestamp>,
    ) -> Result<NotificationPage, ForgeError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use serde_json::json;

    fn event(id: &str, at: &str) -> ForgeEvent {
        ForgeEvent {
            id: id.to_owned(),
            kind: EventKind::Comment,
            repo: RepoRef::new("acme", "widgets"),
            number: 7,
            is_pr: false,
            actor: None,
            body: None,
            url: None,
            timestamp: Timestamp::parse(at).expect("timestamp parses"),
            raw: json!({}),
        }
    }

    #[test]
    fn events_sort_oldest_first_with_id_ties() {
        let page = ForgePage::finish(
            vec![
                event("b", "2026-10-08T00:00:00Z"),
                event("z", "2026-10-07T00:00:00Z"),
                event("a", "2026-10-08T00:00:00Z"),
            ],
            None,
        );
        let ids: Vec<&str> = page.events.iter().map(|event| event.id.as_str()).collect();
        assert_eq!(ids, ["z", "a", "b"]);
    }

    #[test]
    fn the_cursor_never_goes_backwards() {
        // A since newer than everything seen holds: the next poll repeats
        // the boundary instead of re-listing old history.
        let since = Timestamp::parse("2026-10-09T00:00:00Z").expect("parses");
        let page = ForgePage::finish(vec![event("a", "2026-10-08T00:00:00Z")], Some(&since));
        assert_eq!(page.cursor, Some(since));

        let page = ForgePage::finish(
            vec![event("a", "2026-10-10T00:00:00Z")],
            Some(&Timestamp::parse("2026-10-09T00:00:00Z").expect("parses")),
        );
        assert_eq!(
            page.cursor,
            Some(Timestamp::parse("2026-10-10T00:00:00Z").expect("parses"))
        );
    }

    #[test]
    fn empty_pages_keep_or_drop_the_cursor_depending_on_the_anchor() {
        let since = Timestamp::parse("2026-10-09T00:00:00Z").expect("parses");
        let page = ForgePage::finish(vec![], Some(&since));
        assert_eq!(page.cursor, Some(since));

        let page = ForgePage::finish(vec![], None);
        assert_eq!(page.cursor, None);
    }
}
