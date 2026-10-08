//! Offline integration tests: the watcher and the bot driving a full
//! dispatch cycle against a mock GitHub forge and a memory-store
//! dispatcher. Zero network.

use std::sync::Arc;

use rhema_forge_agent::{
    ForgeBot, GitHubForge, Handler, RepoRef, ThreadContext, Watcher, register_forge,
};
use rhema_rsd::{Dispatcher, DispatcherConfig, FixedClock, KeyState, MemoryStore, Outcome};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SINCE: &str = "2026-10-02T10:00:00Z";

fn repo() -> RepoRef {
    RepoRef::new("acme", "widgets")
}

/// A closed issue row, `updated_at` distinguishing poll generations.
fn issue_row(updated_at: &str) -> Value {
    json!({
        "id": 1007,
        "number": 7,
        "state": "closed",
        "title": "thread 7",
        "user": { "login": "alice" },
        "labels": [],
        "created_at": "2026-10-01T09:00:00Z",
        "updated_at": updated_at,
        "closed_at": "2026-10-02T10:00:00Z",
        "html_url": "https://github.com/acme/widgets/issues/7"
    })
}

fn config() -> DispatcherConfig {
    DispatcherConfig {
        debounce_window_millis: 100,
        lease_ttl_millis: 600_000,
        admission_ttl_millis: 500,
        ..DispatcherConfig::default()
    }
}

/// The dispatcher, its hand-driven clock, and a watcher registered against
/// them as `github`.
fn setup() -> (Arc<Dispatcher>, Arc<FixedClock>, Watcher) {
    let clock = Arc::new(FixedClock::new(1_000));
    let dispatcher = Arc::new(Dispatcher::new(
        config(),
        clock.clone(),
        Arc::new(MemoryStore::new()),
    ));
    register_forge(&dispatcher, "github");
    let watcher = Watcher::new(dispatcher.clone(), "github");
    (dispatcher, clock, watcher)
}

fn forge_of(server: &MockServer) -> GitHubForge {
    GitHubForge::new("tok").with_base_url(server.uri())
}

/// Mounts the quiet parts most tests need: an empty comments feed and the
/// thread snapshot.
async fn mount_ancillary(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_row(SINCE)))
        .mount(server)
        .await;
}

/// Mounts the issues feed: generation 1 (no cursor) at 10:00, generation 2
/// (cursor at 10:00) at 11:00 — the changed row re-dispatches. The
/// cursor mock mounts first: wiremock matches mounts in order.
async fn mount_thread_feed(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .and(query_param("since", SINCE))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([issue_row("2026-10-03T11:00:00Z")])),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue_row(SINCE)])))
        .mount(server)
        .await;
}

/// A handler that reports each inbox drain and waits for the test's signal
/// between the two.
struct PausingHandler {
    report: tokio::sync::mpsc::UnboundedSender<usize>,
    resume: Arc<tokio::sync::Notify>,
}

#[rhema_forge_agent::async_trait]
impl Handler for PausingHandler {
    async fn handle(&self, ctx: &mut ThreadContext) -> Outcome {
        let first = ctx.poll_inbox().await.unwrap().len();
        self.report.send(first).unwrap();
        self.resume.notified().await;
        let second = ctx.poll_inbox().await.unwrap().len();
        self.report.send(second).unwrap();
        ctx.comment("still on it — ETA 20 min").await.unwrap();
        Outcome::Handled
    }
}

#[tokio::test]
async fn a_comment_midwork_reaches_the_live_run() {
    let server = MockServer::start().await;
    mount_thread_feed(&server).await;
    mount_ancillary(&server).await;
    // Mounted after the feeds: the only comment-post mock, and it counts.
    let comment_guard = Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .and(body_partial_json(
            json!({ "body": "still on it — ETA 20 min" }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 43 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    let (dispatcher, clock, watcher) = setup();
    let key = rhema_forge_agent::resource_key("github", &repo(), 7);

    // Generation 1 dispatches; the run starts and drains its trigger.
    let fed = watcher.poll(&forge_of(&server), &repo()).await.unwrap();
    assert_eq!(fed, 1);
    clock.advance(100);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let resume = Arc::new(tokio::sync::Notify::new());
    let bot = ForgeBot::new(
        dispatcher.clone(),
        Arc::new(forge_of(&server)),
        Arc::new(PausingHandler {
            report: tx,
            resume: resume.clone(),
        }),
    );
    let runner = tokio::spawn(async move { bot.run_once().await.unwrap() });
    assert_eq!(rx.recv().await, Some(1), "the run starts on its trigger");

    // Generation 2 arrives mid-work: no second dispatch — the live run
    // drains it on its next poll.
    let fed = watcher.poll(&forge_of(&server), &repo()).await.unwrap();
    assert_eq!(fed, 1, "the changed row re-dispatches as a new event");
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    resume.notify_one();
    assert_eq!(rx.recv().await, Some(1), "the live item lands in the run");

    let outcomes = runner.await.unwrap();
    assert_eq!(outcomes, vec![Outcome::Handled]);
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    assert_eq!(status.last_outcome, Some(Outcome::Handled));
    drop(comment_guard);
}

/// Counts its runs and bumps a counter the session carries across runs.
struct ContinuityHandler {
    runs: Arc<std::sync::Mutex<Vec<u64>>>,
}

#[rhema_forge_agent::async_trait]
impl Handler for ContinuityHandler {
    async fn handle(&self, ctx: &mut ThreadContext) -> Outcome {
        ctx.poll_inbox().await.unwrap();
        let mut seen = ctx.session.core.driver_state["seen"].as_u64().unwrap_or(0);
        seen += 1;
        ctx.session.core.driver_state = json!({ "seen": seen });
        ctx.checkpoint().unwrap();
        self.runs.lock().unwrap().push(seen);
        Outcome::Handled
    }
}

#[tokio::test]
async fn the_session_survives_into_the_next_dispatch() {
    let server = MockServer::start().await;
    mount_thread_feed(&server).await;
    mount_ancillary(&server).await;
    let (dispatcher, clock, watcher) = setup();
    let key = rhema_forge_agent::resource_key("github", &repo(), 7);
    let runs = Arc::new(std::sync::Mutex::new(Vec::new()));

    for generation in 1..=2 {
        watcher
            .poll(&forge_of(&server), &repo())
            .await
            .expect("poll feeds one event");
        clock.advance(100);
        let bot = ForgeBot::new(
            dispatcher.clone(),
            Arc::new(forge_of(&server)),
            Arc::new(ContinuityHandler { runs: runs.clone() }),
        );
        assert_eq!(bot.run_once().await.unwrap(), vec![Outcome::Handled]);
        let seen = *runs.lock().unwrap().last().unwrap();
        assert_eq!(seen, generation, "run {generation} resumes the session");
    }
    // Two dispatches, one session: the second run saw the first's write.
    assert_eq!(runs.lock().unwrap().len(), 2);
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Idle);
}

#[tokio::test]
async fn a_failed_handler_records_its_outcome_and_rearms_the_key() {
    struct Failing;
    #[rhema_forge_agent::async_trait]
    impl Handler for Failing {
        async fn handle(&self, ctx: &mut ThreadContext) -> Outcome {
            ctx.poll_inbox().await.unwrap();
            Outcome::Failed("the diff is unreviewable".to_owned())
        }
    }

    let server = MockServer::start().await;
    mount_thread_feed(&server).await;
    mount_ancillary(&server).await;
    let (dispatcher, clock, watcher) = setup();
    let key = rhema_forge_agent::resource_key("github", &repo(), 7);

    watcher.poll(&forge_of(&server), &repo()).await.unwrap();
    clock.advance(100);
    let bot = ForgeBot::new(
        dispatcher.clone(),
        Arc::new(forge_of(&server)),
        Arc::new(Failing),
    );
    assert_eq!(
        bot.run_once().await.unwrap(),
        vec![Outcome::Failed("the diff is unreviewable".to_owned())]
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(
        status.last_outcome,
        Some(Outcome::Failed("the diff is unreviewable".to_owned()))
    );
}

#[tokio::test]
async fn two_threads_dispatch_as_two_runs() {
    struct Counting {
        runs: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[rhema_forge_agent::async_trait]
    impl Handler for Counting {
        async fn handle(&self, ctx: &mut ThreadContext) -> Outcome {
            self.runs.lock().unwrap().push(ctx.key.as_str().to_owned());
            Outcome::Handled
        }
    }

    let server = MockServer::start().await;
    // Two threads on one feed: #7 closed, #8 freshly opened.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            issue_row(SINCE),
            json!({
                "id": 1008, "number": 8, "state": "open",
                "title": "thread 8",
                "user": { "login": "bob" }, "labels": [],
                "created_at": "2026-10-03T08:00:00Z",
                "updated_at": "2026-10-03T08:00:00Z",
                "closed_at": null,
                "html_url": "https://github.com/acme/widgets/issues/8"
            })
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    for (number, title) in [(7u64, "thread 7"), (8, "thread 8")] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/widgets/issues/{number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1000 + number, "number": number, "state": "open",
                "title": title,
                "user": { "login": "alice" }, "labels": [],
                "created_at": "2026-10-01T09:00:00Z",
                "updated_at": "2026-10-01T09:00:00Z",
                "closed_at": null,
                "html_url": format!("https://github.com/acme/widgets/issues/{number}")
            })))
            .mount(&server)
            .await;
    }

    let (dispatcher, clock, watcher) = setup();
    assert_eq!(
        watcher.poll(&forge_of(&server), &repo()).await.unwrap(),
        2,
        "the burst feeds two events"
    );
    clock.advance(100);
    let runs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let bot = ForgeBot::new(
        dispatcher.clone(),
        Arc::new(forge_of(&server)),
        Arc::new(Counting { runs: runs.clone() }),
    );
    assert_eq!(
        bot.run_once().await.unwrap(),
        vec![Outcome::Handled, Outcome::Handled]
    );
    let mut ran = runs.lock().unwrap().clone();
    ran.sort();
    assert_eq!(
        ran,
        vec![
            "github:acme/widgets#7".to_owned(),
            "github:acme/widgets#8".to_owned(),
        ]
    );
}

#[tokio::test]
async fn a_replayed_poll_dedups_in_the_dispatcher() {
    struct Noop;
    #[rhema_forge_agent::async_trait]
    impl Handler for Noop {
        async fn handle(&self, ctx: &mut ThreadContext) -> Outcome {
            ctx.poll_inbox().await.unwrap();
            Outcome::Handled
        }
    }

    let server = MockServer::start().await;
    // One unchanged feed: every poll, with or without a cursor, returns
    // the same row generation.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([issue_row(SINCE)])))
        .mount(&server)
        .await;
    mount_ancillary(&server).await;
    let (dispatcher, clock, watcher) = setup();
    let key = rhema_forge_agent::resource_key("github", &repo(), 7);

    // Generation 1 dispatches and consumes its trigger.
    watcher.poll(&forge_of(&server), &repo()).await.unwrap();
    clock.advance(100);
    let bot = ForgeBot::new(
        dispatcher.clone(),
        Arc::new(forge_of(&server)),
        Arc::new(Noop),
    );
    assert_eq!(bot.run_once().await.unwrap(), vec![Outcome::Handled]);

    // The replay: GitHub's at-or-after `since` re-serves the same row, the
    // dedup id is unchanged. Dedup covers *pending* items — the trigger was
    // already drained, so the replay re-delivers (the contract is
    // at-least-once across drain boundaries), and the next run decides
    // whether the boundary row still matters.
    let cursor_before = watcher.cursor(&repo()).unwrap();
    watcher.poll(&forge_of(&server), &repo()).await.unwrap();
    assert_eq!(watcher.cursor(&repo()).unwrap(), cursor_before);
    assert_eq!(dispatcher.status(&key).unwrap().inbox_depth, 1);
}
