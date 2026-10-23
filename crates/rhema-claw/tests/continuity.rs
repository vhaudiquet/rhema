//! Session continuity across processes: two `--once` passes over the same
//! SQLite db and the same thread. The first pass's conversation is
//! stashed into the dispatcher's `driver_state`; the second pass's agent
//! resumes from it — the MockProvider cannot record its requests, so the
//! stashed state itself is the proof: the restored history carries both
//! exchanges.

use std::sync::Arc;

use rhema_claw::{ClawConfig, run_once};

/// Seeds the watch row straight into the table — the config stopped
/// carrying watches, and the test speaks to the daemon the way an
/// integration would: through the store the routes share.
fn seed_watch(dir: &tempfile::TempDir, host: &str) {
    rhema_claw::ClawStore::open(dir.path().join("dispatch.db"))
        .expect("the watch table opens")
        .add("github", "acme/widgets", Some(host), "tok")
        .expect("the seed row lands");
}
use rhema_core::{Message, MockProvider, ModelOutput, Role, Session, StopReason, Usage};
use rhema_rsd::{KeyState, Outcome, ResourceKey, SqliteStore, Store};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An open issue row, created and updated in one instant: its dedup id
/// `github:issue:1007@…` never changes, so every pass's poll re-feeds it
/// and delivery stays at-least-once — the second pass sees the replay
/// plus the new comment.
fn issue_row() -> Value {
    json!({
        "id": 1007,
        "number": 7,
        "state": "open",
        "title": "crash on save",
        "user": { "login": "alice" },
        "labels": [{ "name": "bug" }],
        "body": "steps to reproduce…",
        "created_at": "2026-10-02T10:00:00Z",
        "updated_at": "2026-10-02T10:00:00Z",
        "closed_at": null,
        "html_url": "https://github.com/acme/widgets/issues/7"
    })
}

/// A comment row on thread 7.
fn comment_row(body: &str) -> Value {
    json!({
        "id": 8001,
        "user": { "login": "dave" },
        "body": body,
        "created_at": "2026-10-03T09:00:00Z",
        "updated_at": "2026-10-03T09:00:00Z",
        "issue_url": "https://api.github.com/repos/acme/widgets/issues/7",
        "html_url": "https://github.com/acme/widgets/issues/7#issuecomment-8001"
    })
}

fn reply(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn config(dir: &tempfile::TempDir) -> ClawConfig {
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
        agent_token: None,
        tools_bin: None,
        transcript: None,
        ingest_token: None,
        calendars: Vec::new(),
    }
}

/// The thread's standing state: open, unchanged across both passes.
async fn mount_snapshot(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_row()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_second_dispatch_resumes_the_first_conversation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue_row()])))
        .mount(&server)
        .await;
    // The comments feed answers per pass: quiet first, then dave's
    // comment. Same matchers, mounted in order — the first serves the
    // first request only, then falls through.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([comment_row("any progress?")])),
        )
        .mount(&server)
        .await;
    mount_snapshot(&server).await;
    let first_post = Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .and(body_partial_json(json!({ "body": "on it" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 43 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let second_post = Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .and(body_partial_json(json!({ "body": "still on it" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 44 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(&dir);
    seed_watch(&dir, &server.uri());
    let key = ResourceKey::from("github:acme/widgets#7");

    // Pass one: the opened event alone. The cron shape — a fresh process,
    // a fresh watcher, the same db.
    let first = run_once(
        &config,
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
    )
    .await
    .expect("the first pass runs");
    assert_eq!(first.detected, 1);
    assert_eq!(first.outcomes, vec![Outcome::Handled]);
    drop(first_post);

    // Pass two: the replayed opened event plus dave's comment, both
    // decoded into one prompt for one resumed run.
    let second = run_once(
        &config,
        mock_registry(Arc::new(MockProvider::new([reply("still on it")]))),
    )
    .await
    .expect("the second pass runs");
    assert_eq!(second.detected, 2, "the replay re-delivers, at-least-once");
    assert_eq!(second.outcomes, vec![Outcome::Handled]);
    drop(second_post);

    // The proof: the second run's stashed session carries the first
    // conversation — a fresh agent would hold only its own exchange.
    let store = SqliteStore::open(dir.path().join("dispatch.db")).expect("db reopens");
    let record = store
        .load(&key)
        .expect("the record loads")
        .expect("the key has a record");
    assert_eq!(record.state, KeyState::Idle);
    let stashed: Session = serde_json::from_value(
        record
            .session
            .expect("the key has a session")
            .core
            .driver_state,
    )
    .expect("the stash parses back into a session");
    let user_prompts: Vec<&str> = stashed
        .history
        .iter()
        .filter_map(|message| match &message.content[0] {
            rhema_core::Content::Text(text) if message.role == Role::User => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_prompts.len(),
        2,
        "both dispatches prompted: {user_prompts:?}"
    );
    assert!(
        user_prompts[0].contains("opened by alice"),
        "pass one's prompt carried the opened event"
    );
    assert!(
        user_prompts[1].contains("any progress?"),
        "pass two's prompt carried the new comment"
    );
    let replies: Vec<&str> = stashed
        .history
        .iter()
        .filter_map(|message| match &message.content[0] {
            rhema_core::Content::Text(text) if message.role == Role::Assistant => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        ["on it", "still on it"],
        "the conversation resumed"
    );
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
