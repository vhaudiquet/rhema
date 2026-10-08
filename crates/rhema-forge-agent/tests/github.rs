//! Offline integration tests: the GitHub backend against an in-process
//! wiremock server. Zero network.

use rhema_forge_agent::{EventKind, Forge, ForgeError, GitHubForge, RepoRef, Timestamp};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SINCE: &str = "2026-10-01T00:00:00Z";

fn forge(server: &MockServer) -> GitHubForge {
    GitHubForge::new("tok").with_base_url(server.uri())
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
            "created_at": "2026-10-01T09:00:00Z",
            "updated_at": "2026-10-01T09:00:00Z",
            "closed_at": Value::Null,
            "html_url": format!("https://github.com/acme/widgets/issues/{number}")
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
async fn poll_pages_the_thread_feed_and_the_comment_feed() {
    let server = MockServer::start().await;
    let page1: Vec<Value> = (0..100)
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
        .and(path("/repos/acme/widgets/issues"))
        .and(query_param("page", "2"))
        .and(query_param("since", SINCE))
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
        .and(path("/repos/acme/widgets/issues"))
        .and(query_param("since", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(page1)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .and(query_param("since", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    assert_eq!(page.events.len(), 101);
    assert!(
        page.events
            .windows(2)
            .all(|pair| pair[0].timestamp <= pair[1].timestamp)
    );
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-03T09:00:00Z").expect("parses"))
    );

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 3);
    assert!(received[0].url.as_str().contains("page=1"));
    assert!(received[0].url.as_str().contains("per_page=100"));
    assert!(received[0].url.as_str().contains("sort=updated"));
    assert!(received[0].url.as_str().contains("state=all"));
    assert!(received[1].url.as_str().contains("page=2"));
    assert!(received[2].url.path() == "/repos/acme/widgets/issues/comments");
}

#[tokio::test]
async fn poll_normalizes_state_kinds_pr_distinction_reviews_and_comments() {
    let server = MockServer::start().await;
    // A closed issue, a fresh issue, a reopened issue, an opened PR row,
    // and a closed PR row — the PRs ride the issues feed behind the
    // `pull_request` key.
    let issues = json!([
        issue_row(
            7,
            json!({
                "state": "closed",
                "created_at": "2026-10-01T09:00:00Z",
                "updated_at": "2026-10-02T10:00:00Z",
                "closed_at": "2026-10-02T10:00:00Z",
                "node_id": "MDU6SXNzdWUxMDA3"
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
        with(
            issue_row(
                8,
                json!({
                    "created_at": "2026-10-04T08:00:00Z",
                    "updated_at": "2026-10-04T08:00:00Z",
                    "html_url": "https://github.com/acme/widgets/pull/8"
                })
            ),
            json!({ "pull_request": { "merged_at": Value::Null } })
        ),
        with(
            issue_row(
                9,
                json!({
                    "state": "closed",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-06T09:00:00Z",
                    "closed_at": "2026-10-06T09:00:00Z",
                    "html_url": "https://github.com/acme/widgets/pull/9"
                })
            ),
            json!({ "pull_request": { "merged_at": Value::Null } })
        ),
    ]);
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues))
        .mount(&server)
        .await;
    // PR #8: open and unmerged as a pulls row → Opened.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9008, "number": 8, "state": "open",
            "user": { "login": "alice" },
            "created_at": "2026-10-04T08:00:00Z",
            "updated_at": "2026-10-04T08:00:00Z",
            "closed_at": Value::Null,
            "merged_at": Value::Null,
            "html_url": "https://github.com/acme/widgets/pull/8"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/8/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 81, "user": { "login": "bob" }, "body": "ship it",
              "state": "APPROVED", "submitted_at": "2026-10-05T10:00:00Z",
              "html_url": "https://github.com/acme/widgets/pull/8#pullrequestreview-81" },
            { "id": 82, "user": { "login": "bob" }, "body": "",
              "state": "PENDING", "submitted_at": Value::Null,
              "html_url": "https://github.com/acme/widgets/pull/8#pullrequestreview-82" }
        ])))
        .mount(&server)
        .await;
    // PR #9: closed and merged as a pulls row → Merged.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9009, "number": 9, "state": "closed",
            "user": { "login": "alice" },
            "created_at": "2026-10-01T09:00:00Z",
            "updated_at": "2026-10-06T12:00:00Z",
            "closed_at": "2026-10-06T09:00:00Z",
            "merged_at": "2026-10-06T12:00:00Z",
            "html_url": "https://github.com/acme/widgets/pull/9"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/9/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 91, "user": { "login": "erin" }, "body": "one nit",
              "state": "CHANGES_REQUESTED", "submitted_at": "2026-10-06T13:00:00Z",
              "html_url": "https://github.com/acme/widgets/pull/9#pullrequestreview-91" }
        ])))
        .mount(&server)
        .await;
    // The repo-wide comments feed: one issue comment, one PR comment.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .and(query_param("sort", "created"))
        .and(query_param("direction", "asc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 7001, "user": { "login": "carol" }, "body": "repro?",
              "created_at": "2026-10-02T11:00:00Z",
              "updated_at": "2026-10-02T11:00:00Z",
              "html_url": "https://github.com/acme/widgets/issues/7#issuecomment-7001",
              "issue_url": "https://api.github.com/repos/acme/widgets/issues/7" },
            { "id": 8001, "user": { "login": "dave" }, "body": "left a review",
              "created_at": "2026-10-05T10:30:00Z",
              "updated_at": "2026-10-05T10:30:00Z",
              "html_url": "https://github.com/acme/widgets/pull/8#issuecomment-8001",
              "issue_url": "https://api.github.com/repos/acme/widgets/pulls/8" }
        ])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    // Oldest → newest; the PENDING review is gone.
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
            (EventKind::Opened, 8, true),
            (EventKind::Review, 8, true),
            (EventKind::Comment, 8, true),
            (EventKind::Merged, 9, true),
            (EventKind::Review, 9, true),
            (EventKind::Reopened, 4, false),
        ]
    );
    // Thread events id on the feed row's id + updated time; comment and
    // review events on their own ids.
    assert_eq!(page.events[0].id, "github:issue:1007@2026-10-02T10:00:00Z");
    assert_eq!(page.events[1].id, "github:comment:7001");
    assert_eq!(page.events[3].id, "github:pull:9008@2026-10-04T08:00:00Z");
    assert_eq!(page.events[4].id, "github:review:81");
    // Polls see row snapshots, not transitions: the actor of a thread
    // event is the thread author; reviews carry the reviewer.
    assert_eq!(page.events[0].actor.as_deref(), Some("alice"));
    assert_eq!(page.events[4].actor.as_deref(), Some("bob"));
    assert_eq!(page.events[4].body.as_deref(), Some("ship it"));
    // The untouched wire row rides along for handler context: fields the
    // event does not lift survive; lifted ones do not.
    assert_eq!(page.events[0].raw["node_id"], "MDU6SXNzdWUxMDA3");
    assert!(page.events[0].raw.get("title").is_none());
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-07T09:00:00Z").expect("parses"))
    );
}

#[tokio::test]
async fn posting_a_comment_uses_the_bearer_header_and_the_issues_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
        .mount(&server)
        .await;

    forge(&server)
        .comment(&RepoRef::new("acme", "widgets"), 7, "on it")
        .await
        .expect("comment posts");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    // The token is a header, never part of the URL.
    assert!(!received[0].url.as_str().contains("tok"));
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body, json!({ "body": "on it" }));
}

#[tokio::test]
async fn snapshot_reads_prs_through_the_pulls_row() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_row(7, json!({ "labels": [{ "name": "bug" }] }))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(with(
            issue_row(8, json!({})),
            json!({ "pull_request": { "merged_at": Value::Null } }),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9008, "number": 8, "state": "closed",
            "title": "fix crash",
            "user": { "login": "alice" },
            "labels": [{ "name": "bug" }],
            "created_at": "2026-10-04T08:00:00Z",
            "updated_at": "2026-10-06T12:00:00Z",
            "merged_at": "2026-10-06T12:00:00Z",
            "html_url": "https://github.com/acme/widgets/pull/8"
        })))
        .mount(&server)
        .await;

    let repo = RepoRef::new("acme", "widgets");
    let issue = forge(&server)
        .snapshot(&repo, 7)
        .await
        .expect("issue snapshot");
    assert_eq!(issue.state, rhema_forge_agent::ThreadState::Open);
    assert!(!issue.is_pr);
    assert_eq!(issue.labels, vec!["bug".to_owned()]);
    assert_eq!(issue.author.as_deref(), Some("alice"));

    let pull = forge(&server)
        .snapshot(&repo, 8)
        .await
        .expect("pull snapshot");
    assert_eq!(pull.state, rhema_forge_agent::ThreadState::Merged);
    assert!(pull.is_pr);
    assert_eq!(pull.title, "fix crash");
}

#[tokio::test]
async fn trailing_slashes_in_the_base_url_are_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_row(7, json!({}))))
        .mount(&server)
        .await;

    let forge = GitHubForge::new("tok").with_base_url(format!("{}/", server.uri()));
    forge
        .snapshot(&RepoRef::new("acme", "widgets"), 7)
        .await
        .expect("the trimmed base reaches the same path");
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string(
                json!({
                    "message": "Not Found",
                    "documentation_url": "https://docs.github.com/rest"
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let err = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("404 is an error");
    match err {
        ForgeError::Api { status, body } => {
            assert_eq!(status, 404);
            assert!(body.contains("Not Found"), "got: {body}");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_transport_failure_maps_to_network() {
    // An unroutable address: nothing is listening, so the connect fails.
    let forge = GitHubForge::new("tok").with_base_url("http://127.0.0.1:1");
    let err = forge
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ForgeError::Network(_)), "got: {err:?}");
}
