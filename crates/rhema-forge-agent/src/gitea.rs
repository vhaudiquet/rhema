//! The Gitea adapter: self-hosted, so the base URL is required and points
//! at the instance root (`https://forge.example`); every request path is
//! prefixed with `/api/v1`. Auth is the token in
//! `Authorization: token <token>`.
//!
//! The endpoints mirror GitHub's shape, with the constraints that differ:
//! - Issues come from `GET /repos/{owner}/{repo}/issues` with
//!   `state=all&type=issues&since=<RFC 3339>`; `type=issues` is explicit
//!   so the feed is issues only, whatever the server's default.
//! - PRs come from `GET /repos/{owner}/{repo}/pulls` with
//!   `state=all&sort=leastrecent`. Gitea's pulls list has no time filter,
//!   so the poll pages the whole list and filters `updated_at` client-side
//!   (belt and braces: `leastrecent` already orders oldest-updated first,
//!   and the final page sort is deterministic either way).
//! - Unlike GitHub, Gitea's pulls rows carry the `merged` flag directly,
//!   so no per-PR detail fetch is needed; only reviews fan out per
//!   updated PR (`GET /repos/{owner}/{repo}/pulls/{index}/reviews`, no
//!   repo-wide reviews feed).
//! - Comments come from the repo-wide
//!   `GET /repos/{owner}/{repo}/issues/comments` with `since=`; PR
//!   conversation comments are issue comments, `pull_request_url` set.
//! - The `since` filters and the client-side cutoff match rows
//!   at-or-after the cursor, so boundary rows repeat across polls; stable
//!   ids make the repetition dedupable.
//! - An updated-but-still-open thread with no close history yields no
//!   thread event: the usual cause is a comment, which the comments feed
//!   reports with its own id.
//! - Reviews carry `submitted`, falling back to `created` on servers that
//!   leave it unset; `PENDING` reviews were never submitted and are
//!   skipped.
//! - Pagination is `limit`/`page` until a short page, with a hard page
//!   cap. `limit=50` because Gitea's default response cap is 50 rows.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{ForgeError, ensure_success, required};
use crate::event::{EventKind, ForgeEvent, RepoRef, Snapshot, ThreadState, Timestamp};
use crate::forge::{Forge, ForgePage};

/// Rows requested per page: Gitea's default response cap.
const PER_PAGE: usize = 50;

/// Pages fetched before a server that keeps returning full pages is
/// declared misbehaving rather than polled forever.
const MAX_PAGES: usize = 1000;

/// Gitea backend: an instance base URL, a token, one reqwest client.
///
/// An empty token is accepted but useless for writes; public reads work
/// anonymously.
pub struct GiteaForge {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl GiteaForge {
    /// A forge authenticating with `token` against the instance at
    /// `base_url` (trailing slashes trimmed). Gitea is self-hosted: there
    /// is no default base.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt: a dead forge fails fast instead of
            // stalling the poll loop.
            .connect_timeout(Duration::from_secs(10))
            // Every forge call is a bounded request/response, unlike a
            // model stream, so one overall timeout per request is the
            // right guard.
            .timeout(Duration::from_secs(20))
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for tests
    /// against a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// A GET builder for `path` with auth applied: the token rides only in
    /// the `Authorization` header, never in a URL.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.get(format!("{base}/api/v1{path}"));
        self.auth(request)
    }

    /// A POST builder for `path` with auth applied.
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.post(format!("{base}/api/v1{path}"));
        self.auth(request)
    }

    /// Gitea's token scheme: `Authorization: token <token>`.
    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            return request;
        }
        request.header("Authorization", format!("token {}", self.token))
    }

    /// Follows `limit`/`page` pagination until a short page.
    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Vec<T>, ForgeError> {
        let mut rows = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = query.to_vec();
            query.push(("limit".to_owned(), PER_PAGE.to_string()));
            query.push(("page".to_owned(), page.to_string()));
            let response = self
                .get(path)
                .query(&query)
                .send()
                .await
                .map_err(ForgeError::network)?;
            let batch: Vec<T> = decode(response).await?;
            let full = batch.len() == PER_PAGE;
            rows.extend(batch);
            if !full {
                return Ok(rows);
            }
        }
        Err(ForgeError::Pagination(format!(
            "more than {MAX_PAGES} full pages from {path}"
        )))
    }
}

/// Decodes a response: non-2xx to [`ForgeError::Api`] with the body
/// snippet, undecodable bodies to [`ForgeError::Parse`].
async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ForgeError> {
    let response = ensure_success(response).await?;
    response
        .json()
        .await
        .map_err(|err| ForgeError::Parse(format!("malformed forge response: {err}")))
}

// --- Wire rows (private): the subset each event lifts, plus the
// --- untouched row under `raw`.

/// A Gitea user object.
#[derive(Debug, Deserialize)]
struct WireUser {
    login: Option<String>,
}

/// A label object.
#[derive(Debug, Deserialize)]
struct WireLabel {
    name: Option<String>,
}

/// The `pull_request` meta on an issue row: its presence is the only PR
/// marker there; the fields it carries are not used.
#[derive(Debug, Deserialize)]
struct WirePullMarker {}

/// One row of the issues list.
#[derive(Debug, Deserialize)]
struct WireIssue {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    labels: Vec<WireLabel>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    #[serde(default)]
    closed_at: Option<Timestamp>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    pull_request: Option<WirePullMarker>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// One row of the pulls list; carries the `merged` flag directly.
#[derive(Debug, Deserialize)]
struct WirePull {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    labels: Vec<WireLabel>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    #[serde(default)]
    closed_at: Option<Timestamp>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// One row of a PR's reviews.
#[derive(Debug, Deserialize)]
struct WireReview {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    submitted: Option<Timestamp>,
    #[serde(default)]
    created: Option<Timestamp>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// One row of the repo-wide comments feed.
#[derive(Debug, Deserialize)]
struct WireComment {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    #[serde(default)]
    html_url: Option<String>,
    /// HTML URL of the parent thread; its trailing segment is the thread
    /// number.
    #[serde(default)]
    issue_url: Option<String>,
    /// Set (non-empty) exactly on PR conversation comments.
    #[serde(default)]
    pull_request_url: Option<String>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

// --- Mapping: wire rows -> normalized events. Pure, so the async
// --- transport stays thin and the derivations are unit-testable.

/// The thread event an issues-list row carries, if any.
fn issue_event(row: &WireIssue, repo: &RepoRef) -> Result<Option<ForgeEvent>, ForgeError> {
    let id = required(row.id, "issue id")?;
    let number = required(row.number, "issue number")?;
    let updated = required(row.updated_at.clone(), "issue updated_at")?;
    let created = required(row.created_at.clone(), "issue created_at")?;
    let kind = if row.state.as_deref() == Some("closed") {
        EventKind::Closed
    } else if row.closed_at.is_some() {
        EventKind::Reopened
    } else if created == updated {
        // Never touched since creation: opening is the only update.
        EventKind::Opened
    } else {
        return Ok(None);
    };
    Ok(Some(ForgeEvent {
        id: format!("gitea:issue:{id}@{}", updated.raw()),
        kind,
        repo: repo.clone(),
        number,
        is_pr: false,
        actor: row.user.as_ref().and_then(|user| user.login.clone()),
        body: None,
        url: row.html_url.clone(),
        timestamp: updated,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The thread event a pulls row carries, if any. `merged` wins over the
/// closed state: a merge closes the PR.
fn pull_event(row: &WirePull, repo: &RepoRef) -> Result<Option<ForgeEvent>, ForgeError> {
    let id = required(row.id, "pull id")?;
    let number = required(row.number, "pull number")?;
    let updated = required(row.updated_at.clone(), "pull updated_at")?;
    let created = required(row.created_at.clone(), "pull created_at")?;
    let kind = if row.merged {
        EventKind::Merged
    } else if row.state.as_deref() == Some("closed") {
        EventKind::Closed
    } else if row.closed_at.is_some() {
        EventKind::Reopened
    } else if created == updated {
        EventKind::Opened
    } else {
        return Ok(None);
    };
    Ok(Some(ForgeEvent {
        id: format!("gitea:pull:{id}@{}", updated.raw()),
        kind,
        repo: repo.clone(),
        number,
        is_pr: true,
        actor: row.user.as_ref().and_then(|user| user.login.clone()),
        body: None,
        url: row.html_url.clone(),
        timestamp: updated,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The review event a reviews row carries, if any: never-submitted
/// (`PENDING`) rows and rows older than the cursor are skipped. The
/// timestamp falls back from `submitted` to `created`, which older
/// servers set alone.
fn review_event(
    row: &WireReview,
    repo: &RepoRef,
    number: u64,
    since: Option<&Timestamp>,
) -> Result<Option<ForgeEvent>, ForgeError> {
    if row.state.as_deref() == Some("PENDING") {
        return Ok(None);
    }
    let timestamp = match row.submitted.clone().or_else(|| row.created.clone()) {
        Some(timestamp) => timestamp,
        None => return Ok(None),
    };
    if let Some(since) = since {
        if timestamp < *since {
            return Ok(None);
        }
    }
    let id = required(row.id, "review id")?;
    Ok(Some(ForgeEvent {
        id: format!("gitea:review:{id}"),
        kind: EventKind::Review,
        repo: repo.clone(),
        number,
        is_pr: true,
        actor: row.user.as_ref().and_then(|user| user.login.clone()),
        body: row.body.clone(),
        // Gitea review rows carry no URL of their own.
        url: None,
        timestamp,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The comment event a repo-wide comments row carries.
fn comment_event(row: &WireComment, repo: &RepoRef) -> Result<Option<ForgeEvent>, ForgeError> {
    let id = required(row.id, "comment id")?;
    let issue_url = required(row.issue_url.clone(), "comment issue_url")?;
    let number = parent_number(&issue_url).ok_or_else(|| {
        ForgeError::Parse(format!(
            "comment issue_url without a thread number: {issue_url}"
        ))
    })?;
    let timestamp = required(
        row.updated_at.clone().or_else(|| row.created_at.clone()),
        "comment timestamp",
    )?;
    let is_pr = row
        .pull_request_url
        .as_deref()
        .is_some_and(|url| !url.is_empty());
    Ok(Some(ForgeEvent {
        id: format!("gitea:comment:{id}"),
        kind: EventKind::Comment,
        repo: repo.clone(),
        number,
        is_pr,
        actor: row.user.as_ref().and_then(|user| user.login.clone()),
        body: row.body.clone(),
        url: row.html_url.clone(),
        timestamp,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The parent thread number: a forge thread URL's trailing segment.
fn parent_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// The snapshot an issues-list row yields.
fn snapshot_from_issue(row: &WireIssue) -> Result<Snapshot, ForgeError> {
    Ok(Snapshot {
        title: required(row.title.clone(), "issue title")?,
        body: row.body.clone(),
        state: if row.state.as_deref() == Some("closed") {
            ThreadState::Closed
        } else {
            ThreadState::Open
        },
        is_pr: false,
        author: row.user.as_ref().and_then(|user| user.login.clone()),
        labels: row
            .labels
            .iter()
            .filter_map(|label| label.name.clone())
            .collect(),
        url: row.html_url.clone(),
        raw: Value::Object(row.raw.clone()),
    })
}

/// The snapshot a pulls row yields; the `merged` flag is authoritative.
fn snapshot_from_pull(row: &WirePull) -> Result<Snapshot, ForgeError> {
    Ok(Snapshot {
        title: required(row.title.clone(), "pull title")?,
        body: row.body.clone(),
        state: if row.merged {
            ThreadState::Merged
        } else if row.state.as_deref() == Some("closed") {
            ThreadState::Closed
        } else {
            ThreadState::Open
        },
        is_pr: true,
        author: row.user.as_ref().and_then(|user| user.login.clone()),
        labels: row
            .labels
            .iter()
            .filter_map(|label| label.name.clone())
            .collect(),
        url: row.html_url.clone(),
        raw: Value::Object(row.raw.clone()),
    })
}

#[async_trait]
impl Forge for GiteaForge {
    async fn poll(
        &self,
        repo: &RepoRef,
        since: Option<&Timestamp>,
    ) -> Result<ForgePage, ForgeError> {
        let mut events = Vec::new();

        // Issue feed: server-side `since`, issues only.
        let issues_path = format!("/repos/{}/{}/issues", repo.owner, repo.name);
        let mut query = vec![
            ("state".to_owned(), "all".to_owned()),
            ("type".to_owned(), "issues".to_owned()),
        ];
        if let Some(since) = since {
            query.push(("since".to_owned(), since.raw().to_owned()));
        }
        for row in self.paged::<WireIssue>(&issues_path, &query).await? {
            if let Some(event) = issue_event(&row, repo)? {
                events.push(event);
            }
        }

        // Pull feed: no server-side time filter exists, so the poll pages
        // the whole list and cuts off client-side.
        let pulls_path = format!("/repos/{}/{}/pulls", repo.owner, repo.name);
        let query = vec![
            ("state".to_owned(), "all".to_owned()),
            ("sort".to_owned(), "leastrecent".to_owned()),
        ];
        for row in self.paged::<WirePull>(&pulls_path, &query).await? {
            let updated = required(row.updated_at.clone(), "pull updated_at")?;
            if let Some(since) = since {
                if updated < *since {
                    continue;
                }
            }
            let number = required(row.number, "pull number")?;
            if let Some(event) = pull_event(&row, repo)? {
                events.push(event);
            }
            // Reviews fan out per updated PR: no repo-wide feed.
            let reviews_path =
                format!("/repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.name);
            for review in self.paged::<WireReview>(&reviews_path, &[]).await? {
                if let Some(event) = review_event(&review, repo, number, since)? {
                    events.push(event);
                }
            }
        }

        // Comment feed, repo-wide: covers issue and PR conversation
        // comments in one shape.
        let comments_path = format!("/repos/{}/{}/issues/comments", repo.owner, repo.name);
        let mut query = Vec::new();
        if let Some(since) = since {
            query.push(("since".to_owned(), since.raw().to_owned()));
        }
        for row in self.paged::<WireComment>(&comments_path, &query).await? {
            if let Some(event) = comment_event(&row, repo)? {
                events.push(event);
            }
        }

        Ok(ForgePage::finish(events, since))
    }

    async fn comment(&self, repo: &RepoRef, number: u64, body: &str) -> Result<(), ForgeError> {
        // The issues endpoint covers PRs too: every PR is an issue here.
        let path = format!(
            "/repos/{}/{}/issues/{number}/comments",
            repo.owner, repo.name
        );
        let response = self
            .post(&path)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(ForgeError::network)?;
        ensure_success(response).await?;
        Ok(())
    }

    async fn snapshot(&self, repo: &RepoRef, number: u64) -> Result<Snapshot, ForgeError> {
        let issue_path = format!("/repos/{}/{}/issues/{number}", repo.owner, repo.name);
        let response = self
            .get(&issue_path)
            .send()
            .await
            .map_err(ForgeError::network)?;
        let issue: WireIssue = decode(response).await?;
        if issue.pull_request.is_some() {
            // The issue view's PR meta is a marker; the pulls row carries
            // the authoritative merged flag.
            let pull_path = format!("/repos/{}/{}/pulls/{number}", repo.owner, repo.name);
            let response = self
                .get(&pull_path)
                .send()
                .await
                .map_err(ForgeError::network)?;
            let pull: WirePull = decode(response).await?;
            Ok(snapshot_from_pull(&pull)?)
        } else {
            Ok(snapshot_from_issue(&issue)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef::new("acme", "widgets")
    }

    fn row(json: &str) -> WireIssue {
        serde_json::from_str(json).expect("issue fixture parses")
    }

    #[test]
    fn a_closed_issue_row_maps_to_closed() {
        let event = issue_event(
            &row(r#"{"id": 107, "number": 7, "state": "closed",
                    "user": {"login": "alice"},
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-02T10:00:00Z",
                    "closed_at": "2026-10-02T10:00:00Z",
                    "html_url": "https://forge.example/acme/widgets/issues/7"}"#),
            &repo(),
        )
        .expect("maps")
        .expect("closed row carries an event");
        assert_eq!(event.kind, EventKind::Closed);
        assert_eq!(event.id, "gitea:issue:107@2026-10-02T10:00:00Z");
        assert!(!event.is_pr);
    }

    #[test]
    fn reopen_is_derived_from_a_close_in_the_past() {
        let event = issue_event(
            &row(r#"{"id": 104, "number": 4, "state": "open",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-07T09:00:00Z",
                    "closed_at": "2026-10-03T00:00:00Z"}"#),
            &repo(),
        )
        .expect("maps")
        .expect("reopened row carries an event");
        assert_eq!(event.kind, EventKind::Reopened);
    }

    #[test]
    fn an_updated_open_row_without_close_history_emits_nothing() {
        let event = issue_event(
            &row(r#"{"id": 106, "number": 6, "state": "open",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-05T09:00:00Z",
                    "closed_at": null}"#),
            &repo(),
        )
        .expect("maps");
        assert_eq!(event, None);
    }

    fn pull(json: &str) -> WirePull {
        serde_json::from_str(json).expect("pull fixture parses")
    }

    #[test]
    fn merged_beats_closed_on_a_pull_row() {
        let merged = pull_event(
            &pull(
                r#"{"id": 203, "number": 3, "state": "closed", "merged": true,
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"}"#,
            ),
            &repo(),
        )
        .expect("maps")
        .expect("merged pull carries an event");
        assert_eq!(merged.kind, EventKind::Merged);
        assert_eq!(merged.id, "gitea:pull:203@2026-10-06T12:00:00Z");

        let closed = pull_event(
            &pull(
                r#"{"id": 204, "number": 4, "state": "closed", "merged": false,
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"}"#,
            ),
            &repo(),
        )
        .expect("maps")
        .expect("closed pull carries an event");
        assert_eq!(closed.kind, EventKind::Closed);
    }

    #[test]
    fn review_timestamp_falls_back_to_created() {
        let review: WireReview = serde_json::from_str(
            r#"{"id": 31, "state": "APPROVED", "submitted": "2026-10-06T13:00:00Z",
                "created": "2026-10-06T13:00:00Z"}"#,
        )
        .expect("review fixture parses");
        let event = review_event(&review, &repo(), 3, None)
            .expect("maps")
            .expect("review carries an event");
        assert_eq!(
            event.timestamp,
            Timestamp::parse("2026-10-06T13:00:00Z").expect("parses")
        );

        // Older servers set `created` alone.
        let bare: WireReview = serde_json::from_str(
            r#"{"id": 33, "state": "COMMENT", "created": "2026-10-06T14:00:00Z"}"#,
        )
        .expect("review fixture parses");
        let event = review_event(&bare, &repo(), 3, None)
            .expect("maps")
            .expect("review carries an event");
        assert_eq!(
            event.timestamp,
            Timestamp::parse("2026-10-06T14:00:00Z").expect("parses")
        );

        // Neither timestamp: never treatable as submitted.
        let timeless: WireReview = serde_json::from_str(r#"{"id": 34, "state": "COMMENT"}"#)
            .expect("review fixture parses");
        assert_eq!(
            review_event(&timeless, &repo(), 3, None).expect("maps"),
            None
        );
    }

    #[test]
    fn a_pr_comment_is_detected_from_its_urls() {
        let comment: WireComment = serde_json::from_str(
            r#"{"id": 3001, "user": {"login": "erin"}, "body": "reviewed",
                "created_at": "2026-10-06T14:00:00Z",
                "updated_at": "2026-10-06T14:00:00Z",
                "issue_url": "https://forge.example/acme/widgets/issues/3",
                "pull_request_url": "https://forge.example/acme/widgets/pulls/3"}"#,
        )
        .expect("comment fixture parses");
        let event = comment_event(&comment, &repo())
            .expect("maps")
            .expect("comment carries an event");
        assert_eq!(event.kind, EventKind::Comment);
        assert!(event.is_pr);
        assert_eq!(event.number, 3);

        // An issue comment carries no pull_request_url.
        let issue_comment: WireComment = serde_json::from_str(
            r#"{"id": 7001, "created_at": "2026-10-02T11:00:00Z",
                "updated_at": "2026-10-02T11:00:00Z",
                "issue_url": "https://forge.example/acme/widgets/issues/7",
                "pull_request_url": null}"#,
        )
        .expect("comment fixture parses");
        let event = comment_event(&issue_comment, &repo())
            .expect("maps")
            .expect("comment carries an event");
        assert!(!event.is_pr);
    }
}
