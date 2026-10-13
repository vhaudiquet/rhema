//! Offline end-to-end: one `--once` pass over a wiremock GitHub — the
//! feed is polled, the MockProvider's reply lands as the thread's
//! comment, and the dispatch state persists in the SQLite db. Zero
//! network.

use std::sync::Arc;

use rhema_claw::{ClawConfig, ForgeKind, Watch, run_once};
use rhema_core::{Message, MockProvider, ModelOutput, Role, StopReason, Usage};
use rhema_rsd::{
    Dispatcher, DispatcherConfig, KeyState, Outcome, ResourceKey, SqliteStore, SystemClock,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A closed issue row, updated at `updated_at`: the feed's event source.
fn issue_row(updated_at: &str) -> Value {
    json!({
        "id": 1007,
        "number": 7,
        "state": "closed",
        "title": "crash on save",
        "user": { "login": "alice" },
        "labels": [{ "name": "bug" }],
        "body": "steps to reproduce…",
        "created_at": "2026-10-01T09:00:00Z",
        "updated_at": updated_at,
        "closed_at": "2026-10-02T10:00:00Z",
        "html_url": "https://github.com/acme/widgets/issues/7"
    })
}

/// The scripted assistant turn: one text reply, nothing else.
fn reply(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// The test's claw config: everything under one tempdir, the watch aimed
/// at the mock server. The provider never resolves through it — the
/// daemon library takes the built provider, and the test builds a mock.
fn config(server: &MockServer, dir: &tempfile::TempDir) -> ClawConfig {
    ClawConfig {
        db: Some(dir.path().join("dispatch.db")),
        workspace_root: Some(dir.path().join("workspaces")),
        memory_root: None,
        bootstrap_root: None,
        poll_interval_secs: 60,
        max_turn_steps: rhema_claw::config::DEFAULT_MAX_TURN_STEPS,
        provider: Some("google".to_owned()),
        model: Some("test-model".to_owned()),
        listen: None,
        token: None,
        watches: vec![Watch {
            forge: ForgeKind::GitHub,
            repo: "acme/widgets".to_owned(),
            host: Some(server.uri()),
            token: Some("tok".to_owned()),
        }],
    }
}

#[tokio::test]
async fn one_pass_polls_reasons_and_posts() {
    let server = MockServer::start().await;
    // The feeds a first poll reads: the issues list (one closed issue)
    // and the quiet comments feed.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([issue_row("2026-10-02T10:00:00Z")])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // The handler's orientation call: the thread snapshot.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_row("2026-10-02T10:00:00Z")))
        .mount(&server)
        .await;
    // The reply, counted: the assertion is that it posts exactly once
    // with the mock's text.
    let comment = Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .and(body_partial_json(json!({ "body": "on it" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 43 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(&server, &dir);
    let provider = Arc::new(MockProvider::new([reply("on it")]));
    let report = run_once(&config, mock_registry(provider))
        .await
        .expect("the pass runs");
    assert_eq!(report.detected, 1, "the feed's one event was fed");
    assert_eq!(report.outcomes, vec![Outcome::Handled]);
    drop(comment);

    // Reopen the db cold: the dispatch state persisted — the key is idle,
    // drained, and carries its outcome.
    let store = SqliteStore::open(dir.path().join("dispatch.db")).expect("db reopens");
    let dispatcher = Dispatcher::new(
        DispatcherConfig::default(),
        Arc::new(SystemClock),
        Arc::new(store),
    );
    let key = ResourceKey::from("github:acme/widgets#7");
    let status = dispatcher.status(&key).expect("status reads");
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    assert_eq!(status.last_outcome, Some(Outcome::Handled));
}

/// A registry whose only backend is the scripted mock — the shape every
/// integration test runs on.
fn mock_registry(provider: Arc<dyn rhema_core::ModelProvider>) -> Arc<rhema_claw::BackendRegistry> {
    Arc::new(
        rhema_claw::BackendRegistry::new(
            vec![rhema_claw::Backend::headless("mock", provider)],
            None,
        )
        .expect("a single mock backend always constructs"),
    )
}
