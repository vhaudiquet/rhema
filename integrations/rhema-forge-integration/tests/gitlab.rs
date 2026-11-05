//! Offline integration tests: the GitLab backend against an in-process
//! wiremock server. Zero network.

use rhema_forge_integration::{
    EventKind, Forge, ForgeError, ForgeThreadKind, GitLabForge, NotificationReason, RepoRef,
    Timestamp,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SINCE: &str = "2026-10-01T00:00:00Z";

fn forge(server: &MockServer) -> GitLabForge {
    GitLabForge::new("tok").with_base_url(server.uri())
}

/// An issues-feed row; `patch` overrides any field.
fn issue_row(iid: i64, patch: Value) -> Value {
    with(
        json!({
            "id": 4000 + iid,
            "iid": iid,
            "state": "opened",
            "title": format!("thread {iid}"),
            "author": { "username": "alice" },
            "labels": [],
            "created_at": "2026-10-01T09:00:00Z",
            "updated_at": "2026-10-01T09:00:00Z",
            "closed_at": Value::Null,
            "web_url": format!("https://gitlab.com/acme/widgets/-/issues/{iid}")
        }),
        patch,
    )
}

/// A merge-requests-feed row; `patch` overrides any field.
fn mr_row(iid: i64, patch: Value) -> Value {
    with(
        json!({
            "id": 5000 + iid,
            "iid": iid,
            "state": "opened",
            "title": format!("mr {iid}"),
            "author": { "username": "bob" },
            "labels": [],
            "created_at": "2026-10-01T08:00:00Z",
            "updated_at": "2026-10-01T08:00:00Z",
            "closed_at": Value::Null,
            "web_url": format!("https://gitlab.com/acme/widgets/-/merge_requests/{iid}")
        }),
        patch,
    )
}

/// A notes row; `patch` overrides any field.
fn note_row(id: i64, created_at: &str, patch: Value) -> Value {
    with(
        json!({
            "id": id,
            "body": "looks broken",
            "system": false,
            "author": { "username": "carol" },
            "created_at": created_at,
            "updated_at": created_at
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
async fn poll_pages_via_x_next_page_and_fans_out_notes() {
    let server = MockServer::start().await;
    // The project path rides the URL percent-encoded.
    // Mount order matters: the page-2 mock must match before the page-1 one.
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues"))
        .and(query_param("page", "2"))
        .and(query_param("updated_after", SINCE))
        .and(header("private-token", "tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue_row(
            4,
            json!({
                "created_at": "2026-10-03T09:00:00Z",
                "updated_at": "2026-10-07T09:00:00Z",
                "closed_at": "2026-10-03T00:00:00Z"
            })
        )])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues"))
        .and(query_param("updated_after", SINCE))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", "2")
                .set_body_json(json!([
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
                    )
                ])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/merge_requests"))
        .and(query_param("updated_after", SINCE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // Notes fan out per updated thread, in feed order.
    for (iid, notes) in [
        (7, json!([note_row(701, "2026-10-02T11:00:00Z", json!({}))])),
        (5, json!([])),
        (4, json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v4/projects/acme%2Fwidgets/issues/{iid}/notes"
            )))
            .and(query_param("sort", "asc"))
            .and(query_param("order_by", "created_at"))
            .respond_with(ResponseTemplate::new(200).set_body_json(notes))
            .mount(&server)
            .await;
    }

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    // Three thread events across the two pages (#7 closed, #5 opened,
    // #4 reopened), plus the note on #7.
    assert_eq!(page.events.len(), 4);
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-07T09:00:00Z").expect("parses"))
    );

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    // Issues pages 1+2, merge requests page 1, then three notes feeds.
    assert_eq!(received.len(), 6);
    assert!(received[0].url.as_str().contains("per_page=100"));
    assert!(received[0].url.as_str().contains("order_by=updated_at"));
    assert!(received[0].url.as_str().contains("sort=asc"));
    assert!(received[1].url.as_str().contains("page=2"));
    assert!(received[2].url.path() == "/api/v4/projects/acme%2Fwidgets/merge_requests");
    assert!(received[3].url.path() == "/api/v4/projects/acme%2Fwidgets/issues/7/notes");
    assert!(received[4].url.path() == "/api/v4/projects/acme%2Fwidgets/issues/5/notes");
    assert!(received[5].url.path() == "/api/v4/projects/acme%2Fwidgets/issues/4/notes");
}

#[tokio::test]
async fn poll_normalizes_state_kinds_and_notes_on_both_feeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            // Closed, with GitLab's fractional-seconds updated stamp.
            issue_row(
                7,
                json!({
                    "state": "closed",
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-02T10:00:00.081Z",
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
                    "created_at": "2026-10-01T09:00:00Z",
                    "updated_at": "2026-10-07T09:00:00Z",
                    "closed_at": "2026-10-03T00:00:00Z"
                })
            )
        ])))
        .mount(&server)
        .await;
    // A merged MR and an MR updated without a transition (no thread event,
    // but its new note still surfaces).
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            mr_row(
                3,
                json!({
                    "state": "merged",
                    "created_at": "2026-10-01T08:00:00Z",
                    "updated_at": "2026-10-06T12:00:00Z",
                    "closed_at": "2026-10-06T12:00:00Z"
                })
            ),
            mr_row(
                9,
                json!({
                    "created_at": "2026-10-04T08:00:00Z",
                    "updated_at": "2026-10-08T09:00:00Z"
                })
            )
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues/7/notes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            // System notes and stale notes are filtered.
            note_row(
                700,
                "2026-10-02T10:00:00Z",
                json!({
                    "body": "changed the description", "system": true
                })
            ),
            note_row(701, "2026-10-02T11:00:00Z", json!({})),
            note_row(702, "2025-01-01T00:00:00Z", json!({ "body": "old" }))
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues/5/notes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues/4/notes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v4/projects/acme%2Fwidgets/merge_requests/3/notes",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([note_row(
            301,
            "2026-10-06T14:00:00Z",
            json!({})
        )])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v4/projects/acme%2Fwidgets/merge_requests/9/notes",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([note_row(
            901,
            "2026-10-08T09:00:00Z",
            json!({})
        )])))
        .mount(&server)
        .await;

    let since = Timestamp::parse(SINCE).expect("cursor parses");
    let page = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), Some(&since))
        .await
        .expect("poll succeeds");

    // Oldest → newest; system and stale notes are gone, and the updated
    // MR without a transition still surfaces its note.
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
            (EventKind::Comment, 3, true),
            (EventKind::Reopened, 4, false),
            (EventKind::Comment, 9, true),
        ]
    );
    // GitLab's fractional seconds survive verbatim in the dedup id.
    assert_eq!(
        page.events[0].id,
        "gitlab:issue:4007@2026-10-02T10:00:00.081Z"
    );
    assert_eq!(page.events[1].id, "gitlab:note:701");
    assert_eq!(page.events[3].id, "gitlab:mr:5003@2026-10-06T12:00:00Z");
    assert_eq!(page.events[5].actor.as_deref(), Some("alice"));
    assert_eq!(page.events[6].actor.as_deref(), Some("carol"));
    assert_eq!(
        page.cursor,
        Some(Timestamp::parse("2026-10-08T09:00:00Z").expect("parses"))
    );
}

#[tokio::test]
async fn an_issue_comment_posts_to_the_issues_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues/4/notes"))
        .and(header("private-token", "tok"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
        .mount(&server)
        .await;

    forge(&server)
        .comment(
            &RepoRef::new("acme", "widgets"),
            ForgeThreadKind::Issue,
            4,
            "on it",
        )
        .await
        .expect("the issue comment posts");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    // One request, to the named sequence only: the merge-requests path
    // is never probed for an issue iid.
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].url.path(),
        "/api/v4/projects/acme%2Fwidgets/issues/4/notes"
    );
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body, json!({ "body": "on it" }));
}

#[tokio::test]
async fn a_merge_request_comment_posts_to_the_merge_requests_path() {
    let server = MockServer::start().await;
    // Only the merge-requests path is mounted: an issues-first probe
    // would 404 here and fail the test, which is the point — the kind
    // must route outright, with no fallback guessing.
    Mock::given(method("POST"))
        .and(path(
            "/api/v4/projects/acme%2Fwidgets/merge_requests/9/notes",
        ))
        .and(header("private-token", "tok"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 43 })))
        .mount(&server)
        .await;

    forge(&server)
        .comment(
            &RepoRef::new("acme", "widgets"),
            ForgeThreadKind::PullRequest,
            9,
            "on the MR",
        )
        .await
        .expect("the MR comment posts");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].url.path(),
        "/api/v4/projects/acme%2Fwidgets/merge_requests/9/notes"
    );
    let body: Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body, json!({ "body": "on the MR" }));
}

#[tokio::test]
async fn an_issue_snapshot_reads_the_issues_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues/7"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issue_row(7, json!({ "labels": ["bug", "critical"] }))),
        )
        .mount(&server)
        .await;

    let issue = forge(&server)
        .snapshot(&RepoRef::new("acme", "widgets"), ForgeThreadKind::Issue, 7)
        .await
        .expect("issue snapshot");
    assert_eq!(issue.state, rhema_forge_integration::ThreadState::Open);
    assert!(!issue.is_pr);
    assert_eq!(issue.labels, vec!["bug".to_owned(), "critical".to_owned()]);

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].url.path(),
        "/api/v4/projects/acme%2Fwidgets/issues/7"
    );
}

#[tokio::test]
async fn a_merge_request_snapshot_reads_the_merge_requests_path() {
    let server = MockServer::start().await;
    // Only the merge-requests path is mounted: the issues path would 404
    // and a fallback would still pass — so the request count pins the
    // no-fallback behavior.
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/merge_requests/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_row(
            3,
            json!({
                "state": "merged",
                "title": "fix crash",
                "labels": ["bug"]
            }),
        )))
        .mount(&server)
        .await;

    let mr = forge(&server)
        .snapshot(
            &RepoRef::new("acme", "widgets"),
            ForgeThreadKind::PullRequest,
            3,
        )
        .await
        .expect("mr snapshot");
    assert_eq!(mr.state, rhema_forge_integration::ThreadState::Merged);
    assert!(mr.is_pr);
    assert_eq!(mr.title, "fix crash");

    let received = server
        .received_requests()
        .await
        .expect("request recording is on");
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].url.path(),
        "/api/v4/projects/acme%2Fwidgets/merge_requests/3"
    );
}

#[tokio::test]
async fn a_non_2xx_response_maps_to_an_api_error_with_the_body_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidgets/issues"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string(json!({ "message": "500 Internal Server Error" }).to_string()),
        )
        .mount(&server)
        .await;

    let err = forge(&server)
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("500 is an error");
    match err {
        ForgeError::Api { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("Internal Server Error"), "got: {body}");
        }
        other => panic!("expected Api error, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_transport_failure_maps_to_network() {
    // An unroutable address: nothing is listening, so the connect fails.
    let forge = GitLabForge::new("tok").with_base_url("http://127.0.0.1:1");
    let err = forge
        .poll(&RepoRef::new("acme", "widgets"), None)
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ForgeError::Network(_)), "got: {err:?}");
}

#[tokio::test]
async fn todos_poll_maps_actions_targets_and_projects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/todos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 501,
                "action_name": "review_requested",
                "target_type": "MergeRequest",
                "target_url": "https://gitlab.com/acme/widgets/-/merge_requests/12",
                "updated_at": "2026-10-05T12:00:00Z",
                "project": { "path_with_namespace": "group/acme/widgets" },
                "target": {
                    "iid": 12,
                    "title": "Add the flange",
                    "author": { "username": "alice" }
                }
            },
            {
                "id": 502,
                "action_name": "assigned",
                "target_type": "Issue",
                "target_url": "https://gitlab.com/acme/widgets/-/issues/3",
                "updated_at": "2026-10-04T08:00:00Z",
                "project": { "path_with_namespace": "acme/widgets" },
                "target": {
                    "iid": 3,
                    "title": "Ship the barn door",
                    "author": { "username": "bob" }
                }
            }
        ])))
        .mount(&server)
        .await;

    let page = forge(&server)
        .notifications(None)
        .await
        .expect("poll succeeds");

    assert_eq!(page.notifications.len(), 2);
    let assignment = &page.notifications[0];
    assert_eq!(
        assignment.id,
        "gitlab:notification:502@2026-10-04T08:00:00Z"
    );
    assert_eq!(assignment.reason, NotificationReason::Assigned);
    assert_eq!(assignment.repo, RepoRef::new("acme", "widgets"));
    assert!(!assignment.is_pr);
    assert_eq!(assignment.actor.as_deref(), Some("bob"));
    let review = &page.notifications[1];
    assert_eq!(review.reason, NotificationReason::ReviewRequested);
    // The namespace path is the owner, the project slug the name.
    assert_eq!(review.repo, RepoRef::new("group/acme", "widgets"));
    assert!(review.is_pr);
    assert_eq!(review.number, 12);
    assert_eq!(
        review.url.as_deref(),
        Some("https://gitlab.com/acme/widgets/-/merge_requests/12")
    );
}
