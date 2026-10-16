//! The GitLab adapter: gitlab.com or self-hosted, at
//! [`DEFAULT_BASE_URL`] — every request path is prefixed with `/api/v4`,
//! and the project is addressed by its URL-encoded full path. Auth is the
//! token in the `PRIVATE-TOKEN` header.
//!
//! Endpoint choices and the constraints that differ:
//! - Issues and merge requests come from
//!   `GET /projects/{encoded path}/{issues,merge_requests}` with
//!   `updated_after=<RFC 3339>&order_by=updated_at&sort=asc`: two separate
//!   feeds, since GitLab has no combined thread feed.
//! - Issue iids and merge-request iids are separate sequences: a number
//!   can exist as both. Wherever a bare number must resolve, the issue
//!   feed is tried first and the merge-request feed is the 404 fallback;
//!   an iid that is neither surfaces the merge-request 404.
//! - GitLab has no repo-wide notes feed and the per-thread notes endpoint
//!   has no updated filter, so every updated thread gets one paginated
//!   `GET .../{issues,merge_requests}/{iid}/notes?sort=asc&order_by=created_at`
//!   call and the `created_at` cutoff is client-side. Note edits do not
//!   re-fire (notes are keyed by creation time).
//! - System notes (state changes, label flips, ...) are skipped: the
//!   thread-level transitions they record already surface from the thread
//!   rows.
//! - `updated_after` matches rows at-or-after the instant, so a re-poll on
//!   the returned cursor repeats boundary rows; stable ids make the
//!   repetition dedupable.
//! - Merge-request state `merged` maps to [`EventKind::Merged`], `closed`
//!   (unmerged) to [`EventKind::Closed`]; issue state `closed` to
//!   [`EventKind::Closed`].
//! - Pagination is offset-based (`per_page`/`page`) with the next page in
//!   the `X-Next-Page` response header. Offset pages over an
//!   updated-ascending sort can shift while a poll runs; duplicates are
//!   absorbed by ids, misses only happen to rows that update twice inside
//!   one poll window.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{ForgeError, ensure_success, required};
use crate::event::{
    EventKind, ForgeEvent, Notification, NotificationPage, NotificationReason, RepoRef, Snapshot,
    ThreadState, Timestamp,
};
use crate::forge::{Forge, ForgePage};

/// The production GitLab base.
pub const DEFAULT_BASE_URL: &str = "https://gitlab.com";

/// Rows requested per page.
const PER_PAGE: usize = 100;

/// Pages fetched before a server that keeps offering pages is declared
/// misbehaving rather than polled forever.
const MAX_PAGES: usize = 1000;

/// GitLab backend: a base URL, a token, one reqwest client.
///
/// An empty token is accepted but useless for writes; public reads work
/// anonymously.
pub struct GitLabForge {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl GitLabForge {
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
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            token: token.into(),
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for
    /// self-hosted instances and for tests against a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// A GET builder for `path` with auth applied: the token rides only in
    /// the `PRIVATE-TOKEN` header, never in a URL.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.get(format!("{base}/api/v4{path}"));
        self.auth(request)
    }

    /// A POST builder for `path` with auth applied.
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let base = &self.base_url;
        let request = self.http.post(format!("{base}/api/v4{path}"));
        self.auth(request)
    }

    /// GitLab's token scheme: the `PRIVATE-TOKEN` header.
    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            return request;
        }
        request.header("PRIVATE-TOKEN", &self.token)
    }

    /// Follows offset pagination via the `X-Next-Page` response header:
    /// absent or empty ends the feed.
    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Vec<T>, ForgeError> {
        let mut rows = Vec::new();
        let mut page: u32 = 1;
        loop {
            let mut query = query.to_vec();
            query.push(("per_page".to_owned(), PER_PAGE.to_string()));
            query.push(("page".to_owned(), page.to_string()));
            let response = self
                .get(path)
                .query(&query)
                .send()
                .await
                .map_err(ForgeError::network)?;
            // The next-page cursor is a header: read it before the body
            // moves.
            let next = response
                .headers()
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            rows.extend(decode::<Vec<T>>(response).await?);
            if next.is_empty() {
                return Ok(rows);
            }
            page = next
                .parse()
                .map_err(|_| ForgeError::Pagination(format!("non-numeric x-next-page: {next}")))?;
            if page > MAX_PAGES as u32 {
                return Err(ForgeError::Pagination(format!(
                    "more than {MAX_PAGES} pages from {path}"
                )));
            }
        }
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

/// The project path (`owner/name`, possibly with subgroups) as GitLab's
/// API needs it: the whole path percent-encoded, slashes included.
fn encode_project_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

// --- Wire rows (private): the subset each event lifts, plus the
// --- untouched row under `raw`.

/// A GitLab author object; the login field is spelled `username` here.
#[derive(Debug, Clone, Deserialize)]
struct WireAuthor {
    username: Option<String>,
}

/// One issue or merge-request row: the same subset lifts from both.
#[derive(Debug, Deserialize)]
struct WireThread {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    iid: Option<u64>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<WireAuthor>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    title: Option<String>,
    /// The opening body: GitLab spells it `description`.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    #[serde(default)]
    closed_at: Option<Timestamp>,
    #[serde(default)]
    web_url: Option<String>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// One note (comment) row of a thread's notes feed.
#[derive(Debug, Deserialize)]
struct WireNote {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    author: Option<WireAuthor>,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// One row of the account's todo list — GitLab's notification feed.
#[derive(Debug, Clone, Deserialize)]
struct WireTodo {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    project: Option<WireTodoProject>,
    #[serde(default)]
    target_type: Option<String>,
    #[serde(default)]
    target: Option<WireTodoTarget>,
    #[serde(default)]
    action_name: Option<String>,
    /// Human-facing URL of the target thread.
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    updated_at: Option<Timestamp>,
    #[serde(flatten)]
    raw: serde_json::Map<String, Value>,
}

/// The project a todo's thread lives in.
#[derive(Debug, Clone, Deserialize)]
struct WireTodoProject {
    #[serde(default)]
    path_with_namespace: Option<String>,
}

/// The thread a todo points at.
#[derive(Debug, Clone, Deserialize)]
struct WireTodoTarget {
    #[serde(default)]
    iid: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    author: Option<WireAuthor>,
}

/// GitLab's `action_name` strings, reduced to the shared vocabulary.
fn gitlab_reason(action: Option<&str>) -> NotificationReason {
    match action {
        Some("review_requested") | Some("approval_required") => NotificationReason::ReviewRequested,
        Some("assigned") => NotificationReason::Assigned,
        Some("mentioned") | Some("directly_addressed") => NotificationReason::Mention,
        Some("build_failed") | Some("build_success") => NotificationReason::CiActivity,
        Some(other) => NotificationReason::Other(other.to_owned()),
        None => NotificationReason::Other("unactioned".to_owned()),
    }
}

/// The notification a todo row carries. The todo list has no server-side
/// time filter, so the poll refetches every pending todo and the dedup
/// ids absorb the repetition; the cursor is advisory only here.
fn todo_notification(row: &WireTodo) -> Result<Option<Notification>, ForgeError> {
    let id = required(row.id, "todo id")?;
    let updated = required(row.updated_at.clone(), "todo updated_at")?;
    let project = required(row.project.clone(), "todo project")?;
    let path = required(project.path_with_namespace, "todo project path")?;
    let (owner, name) = path
        .rsplit_once('/')
        .ok_or_else(|| ForgeError::Parse(format!("todo project path without a slash: {path}")))?;
    let target = required(row.target.clone(), "todo target")?;
    let iid = required(target.iid, "todo target iid")?;
    let is_pr = row.target_type.as_deref() == Some("MergeRequest");
    Ok(Some(Notification {
        id: format!("gitlab:notification:{id}@{}", updated.raw()),
        reason: gitlab_reason(row.action_name.as_deref()),
        repo: RepoRef::new(owner, name),
        number: iid,
        is_pr,
        actor: target
            .author
            .as_ref()
            .and_then(|author| author.username.clone()),
        title: target.title.clone(),
        url: row.target_url.clone(),
        timestamp: updated,
        raw: Value::Object(row.raw.clone()),
    }))
}

// --- Mapping: wire rows -> normalized events. Pure, so the async
// --- transport stays thin and the derivations are unit-testable.

/// The thread event a thread row carries, if any. `merged` only exists on
/// merge requests; a still-open row with a close in its past reopened.
fn thread_event(
    row: &WireThread,
    repo: &RepoRef,
    is_pr: bool,
) -> Result<Option<ForgeEvent>, ForgeError> {
    let id = required(row.id, "thread id")?;
    let iid = required(row.iid, "thread iid")?;
    let updated = required(row.updated_at.clone(), "thread updated_at")?;
    let created = required(row.created_at.clone(), "thread created_at")?;
    let state = row.state.as_deref().unwrap_or_default();
    let kind = if is_pr && state == "merged" {
        EventKind::Merged
    } else if state == "closed" {
        EventKind::Closed
    } else if row.closed_at.is_some() {
        EventKind::Reopened
    } else if created == updated {
        // Never touched since creation: opening is the only update.
        EventKind::Opened
    } else {
        return Ok(None);
    };
    let item = if is_pr { "mr" } else { "issue" };
    Ok(Some(ForgeEvent {
        id: format!("gitlab:{item}:{id}@{}", updated.raw()),
        kind,
        repo: repo.clone(),
        number: iid,
        is_pr,
        actor: row
            .author
            .as_ref()
            .and_then(|author| author.username.clone()),
        body: None,
        url: row.web_url.clone(),
        timestamp: updated,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The comment event a note row carries, if any: system notes record
/// transitions the thread rows already surface, and rows older than the
/// cursor are skipped.
fn note_event(
    row: &WireNote,
    repo: &RepoRef,
    iid: u64,
    is_pr: bool,
    since: Option<&Timestamp>,
) -> Result<Option<ForgeEvent>, ForgeError> {
    if row.system {
        return Ok(None);
    }
    let timestamp = required(row.created_at.clone(), "note created_at")?;
    if let Some(since) = since {
        if timestamp < *since {
            return Ok(None);
        }
    }
    let id = required(row.id, "note id")?;
    Ok(Some(ForgeEvent {
        id: format!("gitlab:note:{id}"),
        kind: EventKind::Comment,
        repo: repo.clone(),
        number: iid,
        is_pr,
        actor: row
            .author
            .as_ref()
            .and_then(|author| author.username.clone()),
        body: row.body.clone(),
        // Note rows carry no URL of their own; the thread's `web_url` is
        // on the thread row.
        url: None,
        timestamp,
        raw: Value::Object(row.raw.clone()),
    }))
}

/// The snapshot a thread row yields.
fn snapshot_from_row(row: &WireThread, is_pr: bool) -> Result<Snapshot, ForgeError> {
    Ok(Snapshot {
        title: required(row.title.clone(), "thread title")?,
        body: row.description.clone(),
        state: match row.state.as_deref() {
            Some("merged") => ThreadState::Merged,
            Some("closed") => ThreadState::Closed,
            _ => ThreadState::Open,
        },
        is_pr,
        author: row
            .author
            .as_ref()
            .and_then(|author| author.username.clone()),
        labels: row.labels.clone(),
        url: row.web_url.clone(),
        raw: Value::Object(row.raw.clone()),
    })
}

#[async_trait]
impl Forge for GitLabForge {
    async fn poll(
        &self,
        repo: &RepoRef,
        since: Option<&Timestamp>,
    ) -> Result<ForgePage, ForgeError> {
        let project = encode_project_path(&repo.path());
        let base = format!("/projects/{project}");
        let mut events = Vec::new();

        // Thread feeds: issues and merge requests, updated-ascending.
        let mut query = vec![
            ("order_by".to_owned(), "updated_at".to_owned()),
            ("sort".to_owned(), "asc".to_owned()),
        ];
        if let Some(since) = since {
            query.push(("updated_after".to_owned(), since.raw().to_owned()));
        }
        let mut threads: Vec<(WireThread, bool)> = Vec::new();
        for row in self
            .paged::<WireThread>(&format!("{base}/issues"), &query)
            .await?
        {
            if let Some(event) = thread_event(&row, repo, false)? {
                events.push(event);
            }
            threads.push((row, false));
        }
        for row in self
            .paged::<WireThread>(&format!("{base}/merge_requests"), &query)
            .await?
        {
            if let Some(event) = thread_event(&row, repo, true)? {
                events.push(event);
            }
            threads.push((row, true));
        }

        // Notes, per updated thread: no repo-wide feed, no updated filter
        // on the endpoint, so each thread's notes are paged and cut off
        // client-side.
        for (row, is_pr) in &threads {
            let iid = required(row.iid, "thread iid")?;
            let segment = if *is_pr { "merge_requests" } else { "issues" };
            let notes_query = vec![
                ("order_by".to_owned(), "created_at".to_owned()),
                ("sort".to_owned(), "asc".to_owned()),
            ];
            let notes_path = format!("{base}/{segment}/{iid}/notes");
            for note in self.paged::<WireNote>(&notes_path, &notes_query).await? {
                if let Some(event) = note_event(&note, repo, iid, *is_pr, since)? {
                    events.push(event);
                }
            }
        }

        Ok(ForgePage::finish(events, since))
    }

    async fn comment(&self, repo: &RepoRef, number: u64, body: &str) -> Result<(), ForgeError> {
        let project = encode_project_path(&repo.path());
        // Issue iids and MR iids are separate sequences: try the issue
        // feed first, fall back on its 404. An iid that is neither
        // surfaces the merge-request 404.
        let notes_path = |segment: &str| format!("/projects/{project}/{segment}/{number}/notes");
        let response = self
            .post(&notes_path("issues"))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(ForgeError::network)?;
        let response = if response.status() == reqwest::StatusCode::NOT_FOUND {
            self.post(&notes_path("merge_requests"))
                .json(&serde_json::json!({ "body": body }))
                .send()
                .await
                .map_err(ForgeError::network)?
        } else {
            response
        };
        ensure_success(response).await?;
        Ok(())
    }

    async fn snapshot(&self, repo: &RepoRef, number: u64) -> Result<Snapshot, ForgeError> {
        let project = encode_project_path(&repo.path());
        let resource = |segment: &str| format!("/projects/{project}/{segment}/{number}");
        let response = self
            .get(&resource("issues"))
            .send()
            .await
            .map_err(ForgeError::network)?;
        // Same iid sequences as `comment`: issues first, 404 fallback.
        let (row, is_pr) = match decode::<WireThread>(response).await {
            Ok(row) => (row, false),
            Err(ForgeError::Api { status: 404, .. }) => {
                let response = self
                    .get(&resource("merge_requests"))
                    .send()
                    .await
                    .map_err(ForgeError::network)?;
                let row: WireThread = decode(response).await?;
                (row, true)
            }
            Err(other) => return Err(other),
        };
        Ok(snapshot_from_row(&row, is_pr)?)
    }

    async fn notifications(
        &self,
        since: Option<&Timestamp>,
    ) -> Result<NotificationPage, ForgeError> {
        // The pending-todo feed: everything addressed to the account,
        // every project it can see. The endpoint has no time filter, so
        // `since` only shapes the returned cursor; the refetch is
        // absorbed by the dedup ids.
        let rows = self.paged::<WireTodo>("/todos", &[]).await?;
        let mut notifications = Vec::new();
        for row in &rows {
            if let Some(notification) = todo_notification(row)? {
                notifications.push(notification);
            }
        }
        Ok(NotificationPage::finish(notifications, since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef::new("acme", "widgets")
    }

    #[test]
    fn project_paths_encode_their_slashes() {
        assert_eq!(encode_project_path("acme/widgets"), "acme%2Fwidgets");
        assert_eq!(encode_project_path("group/sub/proj"), "group%2Fsub%2Fproj");
        // Unreserved characters pass through byte for byte.
        assert_eq!(encode_project_path("safe-name_1.0~x"), "safe-name_1.0~x");
        assert_eq!(encode_project_path("spa ce"), "spa%20ce");
    }

    fn thread(json: &str) -> WireThread {
        serde_json::from_str(json).expect("thread fixture parses")
    }

    #[test]
    fn a_closed_issue_row_maps_to_closed() {
        let event = thread_event(
            &thread(
                r#"{"id": 407, "iid": 7, "state": "closed",
                    "author": {"username": "alice"},
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-02T10:00:00.081Z",
                    "closed_at": "2026-10-02T10:00:00Z",
                    "web_url": "https://gitlab.com/acme/widgets/-/issues/7"}"#,
            ),
            &repo(),
            false,
        )
        .expect("maps")
        .expect("closed row carries an event");
        assert_eq!(event.kind, EventKind::Closed);
        // GitLab's fractional seconds survive verbatim in the id.
        assert_eq!(event.id, "gitlab:issue:407@2026-10-02T10:00:00.081Z");
        assert_eq!(event.number, 7);
        assert!(!event.is_pr);
    }

    #[test]
    fn reopened_and_opened_derive_from_the_close_history() {
        let reopened = thread_event(
            &thread(
                r#"{"id": 404, "iid": 4, "state": "opened",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-07T09:00:00Z",
                    "closed_at": "2026-10-03T00:00:00Z"}"#,
            ),
            &repo(),
            false,
        )
        .expect("maps")
        .expect("reopened row carries an event");
        assert_eq!(reopened.kind, EventKind::Reopened);

        let opened = thread_event(
            &thread(
                r#"{"id": 405, "iid": 5, "state": "opened",
                    "created_at": "2026-10-03T08:00:00Z",
                    "updated_at": "2026-10-03T08:00:00Z",
                    "closed_at": null}"#,
            ),
            &repo(),
            false,
        )
        .expect("maps")
        .expect("fresh row carries an event");
        assert_eq!(opened.kind, EventKind::Opened);
    }

    #[test]
    fn merged_merge_requests_map_to_merged() {
        let event = thread_event(
            &thread(
                r#"{"id": 503, "iid": 3, "state": "merged",
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"}"#,
            ),
            &repo(),
            true,
        )
        .expect("maps")
        .expect("merged row carries an event");
        assert_eq!(event.kind, EventKind::Merged);
        assert!(event.is_pr);
        assert_eq!(event.id, "gitlab:mr:503@2026-10-06T12:00:00Z");

        // Unmerged close on an MR stays Closed.
        let closed = thread_event(
            &thread(
                r#"{"id": 504, "iid": 3, "state": "closed",
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"}"#,
            ),
            &repo(),
            true,
        )
        .expect("maps")
        .expect("closed row carries an event");
        assert_eq!(closed.kind, EventKind::Closed);
    }

    #[test]
    fn system_notes_and_stale_notes_are_skipped() {
        let system: WireNote = serde_json::from_str(
            r#"{"id": 301, "body": "changed the description", "system": true,
                "created_at": "2026-10-02T11:00:00Z"}"#,
        )
        .expect("note fixture parses");
        assert_eq!(
            note_event(&system, &repo(), 7, false, None).expect("maps"),
            None
        );

        let stale: WireNote = serde_json::from_str(
            r#"{"id": 303, "body": "old", "system": false,
                "created_at": "2025-01-01T00:00:00Z"}"#,
        )
        .expect("note fixture parses");
        let since = Timestamp::parse("2026-10-01T00:00:00Z").expect("parses");
        assert_eq!(
            note_event(&stale, &repo(), 7, false, Some(&since)).expect("maps"),
            None
        );
    }

    #[test]
    fn snapshot_states_map_from_the_row_state() {
        let snapshot = snapshot_from_row(
            &thread(
                r#"{"id": 503, "iid": 3, "state": "merged", "title": "fix crash",
                    "description": "the fix", "author": {"username": "bob"},
                    "labels": ["bug", "critical"],
                    "web_url": "https://gitlab.com/acme/widgets/-/merge_requests/3"}"#,
            ),
            true,
        )
        .expect("maps");
        assert_eq!(snapshot.state, ThreadState::Merged);
        assert!(snapshot.is_pr);
        assert_eq!(
            snapshot.labels,
            vec!["bug".to_owned(), "critical".to_owned()]
        );
        assert_eq!(snapshot.author.as_deref(), Some("bob"));
        assert_eq!(snapshot.body.as_deref(), Some("the fix"));
    }
}
