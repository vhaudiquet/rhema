//! Offline integration tests: the Gitea backend against an in-process
//! wiremock server. Zero network.

use rhema_forge_integration::{
    EventKind, Forge, ForgeError, GiteaForge, NotificationReason, RepoRef, Timestamp,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SINCE: &str = "2026-10-01T00:00:00Z";

fn forge(server: &MockServer) -> GiteaForge {
    GiteaForge::new(server.uri(), "tok")
}

/// An issues-list row; `patch` overrides any field.
fn issue_row(number: i64, patch: Value) -> Value {
    with(
        json!({
            "id": 1000 + number,
            "number": number,
            "state": "open",
            "title": format!("thread {number}"),
            "user": { "login": "alice" },
            "labels": [],
            "created_at": "2026-10-01T09:00:00Z",
            "updated_at": "2026-10-01T09:00:00Z",
            "closed_at": Value::Null,
            "html_url": format!("https://forge.example/acme/widgets/issues/{number}"),
            "pull_request": Value::Null
        }),
        patch,
    )
}

/// A pulls-list row; `patch` overrides any field.
fn pull_row(number: i64, patch: Value) -> Value {
    with(
        json!({
            "id": 2000 + number,
            "number": number,
            "state": "open",
            "merged": false,
            "title": format!("pr {number}"),
            "user": { "login": "bob" },
            "labels": [],
            "created_at": "2026-10-01T08:00:00Z",
            "updated_at": "2026-10-01T08:00:00Z",
            "closed_at": Value::Null,
            "html_url": format!("https://forge.example/acme/widgets/pulls/{number}")
        }),
        patch,
    )
}

/// Shallow-merges `patch` into `base`.
fn with(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut base), Value::Object(patch)) => {
            for (key, value) in patch {
                base.insert(key, value);
            }
            Value::Object(base)
        }
        (base, _) => base,
    }
}

#[tokio::test]
async fn poll_pages_the_feeds_with_limit_and_filters_pulls_client_side() {
    let server = MockServer::start().await;
    // A full first page plus a short second one exercises pagination.
    let page1: Vec<Value> = (0..50)
        .map(|i| {
            issue_row(
                i,
                json!({
                    "created_at": "2026-10-02T09:00:00Z",
                    "updated_at": "2026-10-02T09:00:00Z"
                }),
            )
        })
        .collect();
    // Mount order matters: the page-2 mock must match before the page-1 one.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues"))
        .and(query_param("page", "2"))
        .and(query_param("since", SINCE))
        .and(query_param("type", "issues"))
        .and(header("authorization", "token tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue_row(
            200,
            json!({
                "created_at": "2026-10-03T09:00:00Z",
                "updated_at": "2026-10-03T09:00:00Z"
            })
        )])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues"))
        .and(query_param("since", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(page1)))
        .mount(&server)
        .await;
    // The pulls feed has no server-side time filter: it is always paged,
    // and stale rows must be cut off client-side (no reviews fetch for
    // them, no events either).
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls"))
        .and(query_param("sort", "leastrecent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([pull_row(
            55,
            json!({
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            })
        )])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues/comments"))
        .and(query_param("since", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    assert_eq!(page.events.len(), 51);
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-03T09:00:00Z").expect("parses"))
    );

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    // Issues pages 1+2, the pulls page, the comments page — and no
    // reviews fetch for the stale pull.
    assert_eq!(received.len(), 4);
    assert!(received[0].url.as_str().contains("limit=50"));
    assert!(received[0].url.as_str().contains("page=1"));
    assert!(received[1].url.as_str().contains("page=2"));
    assert!(received[2].url.path() == "/api/v1/repos/acme/widgets/pulls");
    assert!(received[3].url.path() == "/api/v1/repos/acme/widgets/issues/comments");
    assert!(
        received
            .iter()
            .all(|request| !request.url.path().contains("reviews"))
    );
}

#[tokio::test]
async fn poll_normalizes_state_kinds_pr_distinction_reviews_and_comments() {
    let server = MockServer::start().await;
    let issues = json!([
        issue_row(
            7,
            json!({
                "state": "closed",
                "created_at": "2026-10-01T09:00:00Z",
                "updated_at": "2026-10-02T10:00:00Z",
                "closed_at": "2026-10-02T10:00:00Z"
            })
        ),
        issue_row(
            5,
            json!({
                "created_at": "2026-10-03T08:00:00Z",
                "updated_at": "2026-10-03T08:00:00Z"
            })
        ),
        issue_row(
            4,
            json!({
                "state": "open",
                "created_at": "2026-10-01T09:00:00Z",
                "updated_at": "2026-10-07T09:00:00Z",
                "closed_at": "2026-10-03T00:00:00Z"
            })
        ),
    ]);
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues))
        .mount(&server)
        .await;
    // A merged PR (updated inside the window) and a stale one.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            pull_row(
                3,
                json!({
                    "state": "closed",
                    "merged": true,
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"
                })
            ),
            pull_row(
                55,
                json!({
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-01T00:00:00Z"
                })
            )
        ])))
        .mount(&server)
        .await;
    // Reviews of the merged PR: one submitted, one never-submitted.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 31, "user": { "login": "carol" }, "body": "lgtm",
              "state": "APPROVED", "official": true, "stale": false,
              "commit_id": "abc123",
              "submitted": "2026-10-06T13:00:00Z",
              "created": "2026-10-06T13:00:00Z",
              "comments": [] },
            { "id": 32, "user": { "login": "carol" }, "body": "",
              "state": "PENDING", "submitted": Value::Null,
              "created": Value::Null, "comments": [] }
        ])))
        .mount(&server)
        .await;
    // The repo-wide comments feed: one issue comment, one PR comment.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 7001, "user": { "login": "dave" }, "body": "repro?",
              "created_at": "2026-10-02T11:00:00Z",
              "updated_at": "2026-10-02T11:00:00Z",
              "html_url": "https://forge.example/acme/widgets/issues/7#comment-7001",
              "issue_url": "https://forge.example/acme/widgets/issues/7",
              "pull_request_url": Value::Null },
            { "id": 3001, "user": { "login": "erin" }, "body": "reviewed",
              "created_at": "2026-10-06T14:00:00Z",
              "updated_at": "2026-10-06T14:00:00Z",
              "html_url": "https://forge.example/acme/widgets/pulls/3#comment-3001",
              "issue_url": "https://forge.example/acme/widgets/issues/3",
              "pull_request_url": "https://forge.example/acme/widgets/pulls/3" }
        ])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    // Oldest → newest; the stale pull and the PENDING review are gone.
    let summary: Vec<(EventKind, u64, bool)> = page
        .events
        .iter()
        .map(|event| (event.kind, event.number, event.is_pr))
        .collect();
    assert_eq!(
        summary,
        vec![
            (EventKind::Closed, 7, false),
            (EventKind::Comment, 7, false),
            (EventKind::Opened, 5, false),
            (EventKind::Merged, 3, true),
            (EventKind::Review, 3, true),
            (EventKind::Comment, 3, true),
            (EventKind::Reopened, 4, false),
        ]
    );
    assert_eq!(page.events[0].id, "gitea:issue:1007@2026-10-02T10:00:00Z");
    assert_eq!(page.events[1].id, "gitea:comment:7001");
    assert_eq!(page.events[3].id, "gitea:pull:2003@2026-10-06T12:00:00Z");
    assert_eq!(page.events[4].id, "gitea:review:31");
    assert_eq!(page.events[4].actor.as_deref(), Some("carol"));
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-07T09:00:00Z").expect("parses"))
    );

    // Gitea's token scheme rides on every request.
    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert!(received.iter().all(|request| {
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some("token tok")
    }));
}

#[tokio::test]
async fn posting_a_comment_uses_the_token_header_and_the_issues_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/acme/widgets/issues/4/comments"))
        .and(header("authorization", "token tok"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
        .mount(&server)
        .await;

    forge(&server)
        .comment(&RepoRef::new("acme", "widgets"), 4, "on it")
        .await
        .expect("comment posts");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert!(!received[0].url.as_str().contains("tok"));
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body, json!({ "body": "on it" }));
}

#[tokio::test]
async fn snapshot_reads_prs_through_the_pulls_row() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues/7"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_row(7, json!({ "labels": [{ "name": "bug" }] }))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues/3"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_row(3, json!({ "pull_request": { "merged": true } }))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pull_row(
            3,
            json!({
                "state": "closed",
                "merged": true,
                "title": "fix crash",
                "labels": [{ "name": "bug" }]
            }),
        )))
        .mount(&server)
        .await;

    let repo = RepoRef::new("acme", "widgets");
    let issue = forge(&server)
        .snapshot(&repo, 7)
        .await
        .expect("issue snapshot");
    assert_eq!(issue.state, rhema_forge_integration::ThreadState::Open);
    assert!(!issue.is_pr);
    assert_eq!(issue.labels, vec!["bug".to_owned()]);

    let pull = forge(&server)
        .snapshot(&repo, 3)
        .await
        .expect("pull snapshot");
    assert_eq!(pull.state, rhema_forge_integration::ThreadState::Merged);
    assert!(pull.is_pr);
    assert_eq!(pull.title, "fix crash");
}

#[tokio::test]
async fn trailing_slashes_in_the_base_url_are_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_row(7, json!({}))))
        .mount(&server)
        .await;

    let forge = GiteaForge::new(format!("{}/", server.uri()), "tok");
    forge
        .snapshot(&RepoRef::new("acme", "widgets"), 7)
        .await
        .expect("the trimmed base reaches the same path");
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widgets/issues"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                json!({
                    "message": "user does not exist [uid: 0, name: ]",
                    "url": "https://forge.example/api/swagger"
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let err = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("401 is an error");
    match err {
        ForgeError::Api { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("user does not exist"), "got: {body}");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_transport_failure_maps_to_network() {
    // An unroutable address: nothing is listening, so the connect fails.
    let forge = GiteaForge::new("http://127.0.0.1:1", "tok");
    let err = forge
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ForgeError::Network(_)), "got: {err:?}");
}

#[tokio::test]
async fn notifications_poll_maps_the_subject_feed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("status-types", "unread"))
        .and(query_param("since", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 91,
                "unread": true,
                "updated_at": "2026-10-05T12:00:00Z",
                "subject": {
                    "title": "Fix the gate",
                    "url": "https://forge.example/api/v1/repos/acme/widgets/pulls/9",
                    "type": "Pull"
                },
                "repository": { "full_name": "acme/widgets" }
            }
        ])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .notifications(Some(&since))
        .await
        .expect("poll succeeds");

    assert_eq!(page.notifications.len(), 1);
    let note = &page.notifications[0];
    assert_eq!(note.id, "gitea:notification:91@2026-10-05T12:00:00Z");
    // Gitea's feed names no per-row reason: the fixed fallback spelling.
    assert_eq!(note.reason, NotificationReason::Other("gitea".to_owned()));
    assert_eq!(note.number, 9);
    assert!(note.is_pr);
    assert_eq!(note.repo, RepoRef::new("acme", "widgets"));
    assert_eq!(note.title.as_deref(), Some("Fix the gate"));
}
