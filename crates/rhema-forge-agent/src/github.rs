//! The GitHub adapter: REST v3, at [`DEFAULT_BASE_URL`] or a GitHub
//! Enterprise Server base. Auth is the token in `Authorization: Bearer` —
//! never a query parameter, so the token never lands in URLs or logs.
//!
//! Endpoint choices and the constraints they pin:
//! - Threads come from `GET /repos/{owner}/{repo}/issues` with
//!   `state=all&sort=updated&direction=asc&since=<RFC 3339>`: the issues
//!   list is the only thread feed with a server-side time filter, and PR
//!   rows ride it behind the `pull_request` key.
//! - The merged flag and reviews are invisible from the issues list: the
//!   pulls list has no time filter and reviews have no repo-wide feed, so
//!   every PR row the issues list returns costs one `GET /pulls/{n}`
//!   (merged detection) and one `GET /pulls/{n}/reviews` (filtered
//!   client-side by `submitted_at`).
//! - Comments come from the repo-wide
//!   `GET /repos/{owner}/{repo}/issues/comments` with
//!   `sort=created&direction=asc&since=`; a PR conversation comment is the
//!   same row shape, its `issue_url` ending in `/pulls/`.
//! - GitHub's `since` matches rows updated at-or-after the instant, so a
//!   re-poll on the returned cursor repeats boundary rows; stable ids make
//!   the repetition dedupable.
//! - An updated-but-still-open thread with no close history yields no
//!   thread event: the usual cause is a comment, which the comments feed
//!   reports with its own id. Title/label edits alone surface as nothing.
//! - A `PENDING` review was never submitted and is skipped; `DISMISSED` is
//!   activity and normalizes as [`EventKind::Review`].
//! - A comment edit changes the row's `updated_at` but not its id: the
//!   edit reappears at the boundary and dedups away. Edits are not
//!   re-dispatched.
//! - Pagination is `per_page`/`page` until a short page, with a hard page
//!   cap. Page offsets over an updated-ascending sort can shift while a
//!   poll is running; duplicates are absorbed by ids, misses only happen
//!   to rows that update twice inside one poll window.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{ForgeError, ensure_success, required};
use crate::event::{EventKind, ForgeEvent, RepoRef, Snapshot, ThreadState, Timestamp};
use crate::forge::{Forge, ForgePage};

/// The production GitHub API base.
pub const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// GitHub rejects requests without a `User-Agent` outright.
const USER_AGENT: &str = concat!("rhema-forge-agent/", env!("CARGO_PKG_VERSION"));

/// Rows requested per page.
const PER_PAGE: usize = 100;

/// Pages fetched before a server that keeps returning full pages is
/// declared misbehaving rather than polled forever.
const MAX_PAGES: usize = 1000;

/// GitHub backend: a base URL, a token, one reqwest client.
///
/// An empty token is accepted but useless for writes; public reads work
/// anonymously.
pub struct GitHubForge {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl GitHubForge {
    /// A forge authenticating with `token` against [`DEFAULT_BASE_URL`].
    pub fn new(token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt: a dead forge fails fast instead of
            // stalling the poll loop.
            .connect_timeout(Duration::from_secs(10))
            // Every forge call is a bounded request/response, unlike a
            // model stream, so one overall timeout per request is the
            // right guard.
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            token: token.into(),
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for GitHub
    /// Enterprise Server and for tests against a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// A GET builder for `path` with auth applied: the token rides only in
    /// the `Authorization: Bearer` header, never in a URL.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.get(format!("{base}{path}"));
        if self.token.is_empty() {
            request
        } else {
            request.bearer_auth(&self.token)
        }
    }

    /// A POST builder for `path` with auth applied.
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.post(format!("{base}{path}"));
        if self.token.is_empty() {
            request
        } else {
            request.bearer_auth(&self.token)
        }
    }

    /// Follows `per_page`/`page` pagination until a short page.
    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Vec<T>, ForgeError> {
        let mut rows = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = query.to_vec();
            query.push(("per_page".to_owned(), PER_PAGE.to_string()));
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

/// A GitHub user object.
#[derive(Debug, Deserialize)]
struct WireUser {
    login: Option<String>,
}

/// A label object.
#[derive(Debug, Deserialize)]
struct WireLabel {
    name: Option<String>,
}

/// The `pull_request` key: its presence is the only PR marker on issue
/// rows; the fields it carries are not used.
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

/// One `GET /pulls/{n}` row.
#[derive(Debug, Deserialize)]
struct WirePull {
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
    merged_at: Option<Timestamp>,
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
    submitted_at: Option<Timestamp>,
    #[serde(default)]
    html_url: Option<String>,
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
    /// API URL of the parent thread; its trailing segment is the thread
    /// number, and `/pulls/` in it marks a PR comment.
    #[serde(default)]
    issue_url: Option<String>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

// --- Mapping: wire rows -> normalized events. Pure, so the async
// --- transport stays thin and the derivations are unit-testable.

/// The thread event an issues-list row carries, if any. State transitions
/// are derivable from the row; other updates are not.
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
        id: format!("github:issue:{id}@{}", updated.raw()),
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

/// The thread event a pulls row carries, if any. The merged flag wins over
/// the closed state: a merge closes the PR.
fn pull_event(row: &WirePull, repo: &RepoRef) -> Result<Option<ForgeEvent>, ForgeError> {
    let id = required(row.id, "pull id")?;
    let number = required(row.number, "pull number")?;
    let updated = required(row.updated_at.clone(), "pull updated_at")?;
    let created = required(row.created_at.clone(), "pull created_at")?;
    let kind = if row.merged_at.is_some() {
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
        id: format!("github:pull:{id}@{}", updated.raw()),
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
/// (`PENDING`) rows and rows older than the cursor are skipped.
fn review_event(
    row: &WireReview,
    repo: &RepoRef,
    number: u64,
    since: Option<&Timestamp>,
) -> Result<Option<ForgeEvent>, ForgeError> {
    if row.state.as_deref() == Some("PENDING") {
        return Ok(None);
    }
    // A review without a submission time was never submitted.
    let timestamp = required(row.submitted_at.clone(), "review submitted_at")?;
    if let Some(since) = since {
        if timestamp < *since {
            return Ok(None);
        }
    }
    let id = required(row.id, "review id")?;
    Ok(Some(ForgeEvent {
        id: format!("github:review:{id}"),
        kind: EventKind::Review,
        repo: repo.clone(),
        number,
        is_pr: true,
        actor: row.user.as_ref().and_then(|user| user.login.clone()),
        body: row.body.clone(),
        url: row.html_url.clone(),
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
    Ok(Some(ForgeEvent {
        id: format!("github:comment:{id}"),
        kind: EventKind::Comment,
        repo: repo.clone(),
        number,
        is_pr: issue_url.contains("/pulls/"),
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

/// The snapshot a pulls row yields; `merged_at` is the authoritative
/// merged flag the issue view of a PR cannot prove.
fn snapshot_from_pull(row: &WirePull) -> Result<Snapshot, ForgeError> {
    Ok(Snapshot {
        title: required(row.title.clone(), "pull title")?,
        body: row.body.clone(),
        state: if row.merged_at.is_some() {
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
impl Forge for GitHubForge {
    async fn poll(
        &self,
        repo: &RepoRef,
        since: Option<&Timestamp>,
    ) -> Result<ForgePage, ForgeError> {
        let mut events = Vec::new();

        // Thread feed: the issues list, the only feed with a server-side
        // time filter. PR rows ride behind the `pull_request` key.
        let threads_path = format!("/repos/{}/{}/issues", repo.owner, repo.name);
        let mut query = vec![
            ("state".to_owned(), "all".to_owned()),
            ("sort".to_owned(), "updated".to_owned()),
            ("direction".to_owned(), "asc".to_owned()),
        ];
        if let Some(since) = since {
            query.push(("since".to_owned(), since.raw().to_owned()));
        }
        let mut pull_numbers = Vec::new();
        for row in self.paged::<WireIssue>(&threads_path, &query).await? {
            if row.pull_request.is_some() {
                pull_numbers.push(required(row.number, "issue number")?);
            } else if let Some(event) = issue_event(&row, repo)? {
                events.push(event);
            }
        }

        // Per updated PR: the merged flag and the reviews are invisible
        // from the issues list.
        for number in pull_numbers {
            let detail_path = format!("/repos/{}/{}/pulls/{number}", repo.owner, repo.name);
            let response = self
                .get(&detail_path)
                .send()
                .await
                .map_err(ForgeError::network)?;
            let pull: WirePull = decode(response).await?;
            if let Some(event) = pull_event(&pull, repo)? {
                events.push(event);
            }
            let reviews = self
                .paged::<WireReview>(&format!("{detail_path}/reviews"), &[])
                .await?;
            for review in &reviews {
                if let Some(event) = review_event(review, repo, number, since)? {
                    events.push(event);
                }
            }
        }

        // Comment feed, repo-wide: covers issue and PR conversation
        // comments in one shape.
        let comments_path = format!("/repos/{}/{}/issues/comments", repo.owner, repo.name);
        let mut query = vec![
            ("sort".to_owned(), "created".to_owned()),
            ("direction".to_owned(), "asc".to_owned()),
        ];
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
            // The issue view of a PR lacks the authoritative merged flag.
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
            &row(r#"{"id": 1007, "number": 7, "state": "closed",
                    "user": {"login": "alice"}, "labels": [],
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-02T10:00:00Z",
                    "closed_at": "2026-10-02T10:00:00Z",
                    "html_url": "https://github.com/acme/widgets/issues/7"}"#),
            &repo(),
        )
        .expect("maps")
        .expect("closed row carries an event");
        assert_eq!(event.kind, EventKind::Closed);
        assert_eq!(event.id, "github:issue:1007@2026-10-02T10:00:00Z");
        assert_eq!(event.number, 7);
        assert!(!event.is_pr);
        assert_eq!(event.actor.as_deref(), Some("alice"));
        assert_eq!(event.raw["title"], serde_json::Value::Null);
    }

    #[test]
    fn reopen_is_derived_from_a_close_in_the_past() {
        let event = issue_event(
            &row(r#"{"id": 1004, "number": 4, "state": "open",
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
            &row(r#"{"id": 1006, "number": 6, "state": "open",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-05T09:00:00Z",
                    "closed_at": null}"#),
            &repo(),
        )
        .expect("maps");
        assert_eq!(event, None);
    }

    #[test]
    fn a_row_missing_a_required_field_is_a_parse_error() {
        let err = issue_event(&row(r#"{"number": 7, "state": "closed"}"#), &repo())
            .expect_err("missing id is loud");
        assert!(matches!(err, ForgeError::Parse(_)), "got: {err:?}");
    }

    fn pull(json: &str) -> WirePull {
        serde_json::from_str(json).expect("pull fixture parses")
    }

    #[test]
    fn merged_beats_closed_on_a_pull_row() {
        let merged = pull_event(
            &pull(
                r#"{"id": 9009, "number": 9, "state": "closed",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z",
                    "merged_at": "2026-10-06T12:00:00Z"}"#,
            ),
            &repo(),
        )
        .expect("maps")
        .expect("merged pull carries an event");
        assert_eq!(merged.kind, EventKind::Merged);
        assert!(merged.is_pr);
        assert_eq!(merged.id, "github:pull:9009@2026-10-06T12:00:00Z");

        let closed = pull_event(
            &pull(
                r#"{"id": 9010, "number": 10, "state": "closed",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z",
                    "merged_at": null}"#,
            ),
            &repo(),
        )
        .expect("maps")
        .expect("closed pull carries an event");
        assert_eq!(closed.kind, EventKind::Closed);
    }

    #[test]
    fn pending_reviews_without_submission_are_skipped() {
        let review: WireReview =
            serde_json::from_str(r#"{"id": 82, "state": "PENDING", "submitted_at": null}"#)
                .expect("review fixture parses");
        let event = review_event(&review, &repo(), 8, None).expect("maps");
        assert_eq!(event, None);
    }

    #[test]
    fn a_pr_comment_is_detected_from_its_issue_url() {
        let comment: WireComment = serde_json::from_str(
            r#"{"id": 8001, "user": {"login": "dave"}, "body": "looks",
                "created_at": "2026-10-05T10:30:00Z",
                "updated_at": "2026-10-05T10:30:00Z",
                "issue_url": "https://api.github.com/repos/acme/widgets/pulls/8"}"#,
        )
        .expect("comment fixture parses");
        let event = comment_event(&comment, &repo())
            .expect("maps")
            .expect("comment carries an event");
        assert_eq!(event.kind, EventKind::Comment);
        assert!(event.is_pr);
        assert_eq!(event.number, 8);
        assert_eq!(event.id, "github:comment:8001");
    }

    #[test]
    fn a_comment_url_without_a_number_is_a_parse_error() {
        let comment: WireComment = serde_json::from_str(
            r#"{"id": 8002, "issue_url": "https://api.github.com/repos/acme/widgets/issues/"}"#,
        )
        .expect("comment fixture parses");
        let err = comment_event(&comment, &repo()).expect_err("loud failure");
        assert!(matches!(err, ForgeError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn the_raw_payload_keeps_the_fields_the_event_does_not_lift() {
        let event = issue_event(
            &row(
                r#"{"id": 1007, "number": 7, "state": "closed", "title": "crash",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-02T10:00:00Z",
                    "closed_at": "2026-10-02T10:00:00Z",
                    "node_id": "MDU6SXNzdWUxMDA3"}"#,
            ),
            &repo(),
        )
        .expect("maps")
        .expect("closed row carries an event");
        // Undeclared keys survive; the lifted ones (id, number, state,
        // title, ...) do not.
        assert_eq!(event.raw["node_id"], "MDU6SXNzdWUxMDA3");
        assert!(event.raw.get("title").is_none());
    }
}
