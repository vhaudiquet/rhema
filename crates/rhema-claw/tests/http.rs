//! The chat surface, offline and end to end: a daemon over an ephemeral
//! HTTP port, a MockProvider scripted with the replies, wiremock for the
//! forge — zero network. The routes are exercised as a stock client
//! would: subscribe the event stream, POST a message, watch the turn go
//! by; add and remove watches over HTTP and watch the next detect pass
//! pick them up; talk `/v1/chat/completions` both streamed and whole;
//! feed `/v1/ingest` and poll the notification queue; and verify a
//! bearer token guards `/v1` while `/healthz` stays open.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rhema_autonomous_agent::Allowlist;
use rhema_claw::watch_tools;
use rhema_claw::{ClawConfig, Daemon, ForgeKind, Watch};
use rhema_core::{Content, Message, MockProvider, ModelOutput, Role, Session, StopReason, Usage};
use rhema_rsd::{ResourceKey, SqliteStore, Store};
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The scripted assistant turn: one text reply, nothing else.
fn reply(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// The test's claw config: everything under one tempdir, no seed
/// watches, no listen address of its own (the test binds ephemeral).
fn config(dir: &tempfile::TempDir, token: Option<&str>) -> ClawConfig {
    ClawConfig {
        db: Some(dir.path().join("dispatch.db")),
        workspace_root: Some(dir.path().join("workspaces")),
        memory_root: None,
        bootstrap_root: Some(dir.path().join("bootstrap")),
        poll_interval_secs: 60,
        provider: Some("google".to_owned()),
        model: Some("test-model".to_owned()),
        listen: None,
        token: token.map(str::to_owned),
        watches: Vec::new(),
    }
}

/// Binds the daemon's router on an ephemeral loopback port and returns
/// the base URL.
async fn serve(daemon: &Daemon) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the port binds");
    let addr = listener.local_addr().expect("the address reads");
    let app = daemon.router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Reads the SSE body until `needle` appears in the accumulated text.
/// The stream never ends on its own (a chat UI subscribes once and
/// chats forever), so the deadline is the backstop.
type SseBody = Pin<Box<dyn tokio_stream::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

async fn read_until(stream: &mut SseBody, seen: &mut String, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen.contains(needle) {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for the stream to carry the needle")
            .expect("the stream ended early")
            .expect("the body errored");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// Pins and returns the SSE body of a streaming GET.
async fn sse_body(response: reqwest::Response) -> SseBody {
    Box::pin(response.bytes_stream())
}

/// One quiet GitHub repo, for the watches test: an issues list with a
/// single closed issue and a quiet comments feed.
async fn mount_github(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 1007,
            "number": 7,
            "state": "closed",
            "title": "crash on save",
            "user": { "login": "alice" },
            "labels": [],
            "body": "steps to reproduce…",
            "created_at": "2026-10-01T09:00:00Z",
            "updated_at": "2026-10-02T10:00:00Z",
            "closed_at": "2026-10-02T10:00:00Z",
            "html_url": "https://github.com/acme/widgets/issues/7"
        }])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
}

#[tokio::test]
async fn chat_streams_turns_and_remembers_across_messages() {
    let provider = Arc::new(MockProvider::new([reply("on it"), reply("still on it")]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider.clone()),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // The subscriber comes first: a chat UI subscribes once and chats
    // forever.
    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    assert_eq!(events.status(), 200);
    let content_type = events
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("a content type")
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let mut stream = sse_body(events).await;
    let mut seen = String::new();

    // Inventory before anything happened: no keys have records.
    let sessions: Vec<Value> = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    assert!(sessions.is_empty(), "{sessions:?}");

    // Message one: accepted, and the turn streams.
    let accepted = client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "hello"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202);
    assert_eq!(
        accepted.json::<Value>().await.expect("body parses"),
        json!({"key": "interactive:web", "accepted": true})
    );
    read_until(&mut stream, &mut seen, "on it").await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    assert!(
        seen.contains(r#""type":"text_delta","text":"on it""#),
        "the reply streamed as a delta: {seen}"
    );
    assert!(
        seen.contains(r#""reply":"on it","outcome":"handled""#),
        "the end marker carries the reply: {seen}"
    );

    // Message two: same key, same stream — and the provider gets its
    // SECOND scripted reply only if the conversation resumed.
    client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "again"}))
        .send()
        .await
        .expect("the second message posts")
        .error_for_status()
        .expect("the second message accepts");
    read_until(&mut stream, &mut seen, "still on it").await;

    // The proof, cold: the store's driver_state carries both exchanges.
    let store = SqliteStore::open(dir.path().join("dispatch.db")).expect("db reopens");
    let key = ResourceKey::from("interactive:web");
    let record = store
        .load(&key)
        .expect("the record loads")
        .expect("the key has a record");
    let stashed: Session =
        serde_json::from_value(record.session.expect("a session").core.driver_state)
            .expect("the stash parses");
    let turns: Vec<(Role, &str)> = stashed
        .history
        .iter()
        .filter_map(|message| match &message.content[0] {
            Content::Text(text) => Some((message.role, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        turns,
        [
            (Role::User, "hello"),
            (Role::Assistant, "on it"),
            (Role::User, "again"),
            (Role::Assistant, "still on it"),
        ],
        "the conversation resumed across messages"
    );

    // The session is now in the inventory with its last outcome.
    let sessions: Vec<Value> = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0]["key"], "interactive:web");
    assert_eq!(sessions[0]["state"], "idle");
    assert_eq!(sessions[0]["inbox_depth"], 0);
    assert_eq!(sessions[0]["last_outcome"], "Handled");

    // Non-interactive keys are refused. The key travels percent-encoded:
    // a raw slash would leave the single-segment route.
    let refused = client
        .post(format!(
            "{base}/v1/sessions/github%3Aacme%2Fwidgets%237/messages"
        ))
        .json(&json!({"text": "hello"}))
        .send()
        .await
        .expect("the forged key posts");
    assert_eq!(refused.status(), 400, "only resident keys take messages");
}

/// The other two resident prefixes take chat messages, and their turns'
/// replies auto-notify: a `phone:` key delivers to the device its name
/// carries, a `task:` key to the phone. Forge keys stay refused.
#[tokio::test]
async fn phone_and_task_keys_take_messages_and_auto_notify_their_replies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it"), reply("done")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // A phone-surface message: accepted like a chat message.
    let accepted = client
        .post(format!("{base}/v1/sessions/phone%3Aana/messages"))
        .json(&json!({"text": "is CI green?"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202, "phone: keys take messages");

    // The reply had no subscriber: it landed in the notification table,
    // on the device the key names.
    let listed: Value = client
        .get(format!("{base}/v1/notifications?device=ana"))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    let rows = listed["notifications"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["body"], "on it");
    assert_eq!(rows[0]["device"], "ana");
    assert_eq!(rows[0]["title"], "claw");

    // A task-surface message notifies the phone: the key names no
    // device, so the default answers.
    client
        .post(format!("{base}/v1/sessions/task%3A7/messages"))
        .json(&json!({"text": "check the bread"}))
        .send()
        .await
        .expect("the message posts")
        .error_for_status()
        .expect("task: keys take messages");
    let listed: Value = client
        .get(format!("{base}/v1/notifications"))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    let rows = listed["notifications"].as_array().expect("rows");
    assert_eq!(
        rows.iter()
            .map(|row| row["body"].as_str())
            .collect::<Vec<_>>(),
        [Some("done")],
        "the task reply is the only outstanding phone row: {listed}"
    );
}

#[tokio::test]
async fn watches_added_over_http_feed_the_next_detect_pass() {
    let server = MockServer::start().await;
    mount_github(&server).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        // One scripted reply, in case a forge run starts; this test
        // asserts detection only.
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // Empty config, empty table: nothing detects.
    let report = daemon.tick().await.expect("the pass runs");
    assert_eq!(report.detected, 0);

    // Add a watch over HTTP, aimed at the wiremock.
    let added = client
        .post(format!("{base}/v1/watches"))
        .json(&json!({
            "forge": "github",
            "repo": "acme/widgets",
            "host": server.uri(),
            "token": "tok",
        }))
        .send()
        .await
        .expect("the watch posts");
    assert_eq!(added.status(), 201, "a new watch is created");
    assert_eq!(
        added.json::<Value>().await.expect("body parses"),
        json!({"added": true, "forge": "github", "repo": "acme/widgets"})
    );

    // The table answers the list route, tokens never on the wire.
    let listed: Value = client
        .get(format!("{base}/v1/watches"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses");
    let watch = &listed["watches"][0];
    assert_eq!(watch["forge"], "github");
    assert_eq!(watch["repo"], "acme/widgets");
    assert_eq!(watch["host"], server.uri());
    assert!(
        watch.get("token").is_none(),
        "the token is not serialized: {listed}"
    );

    // The next pass detects through the new watch — no restart.
    let report = daemon.tick().await.expect("the pass runs");
    assert_eq!(report.detected, 1, "the new watch's feed event was fed");

    // A re-add is a no-op with a 200, not a duplicate row.
    let re_added = client
        .post(format!("{base}/v1/watches"))
        .json(&json!({"forge": "github", "repo": "acme/widgets", "host": server.uri()}))
        .send()
        .await
        .expect("the re-add posts");
    assert_eq!(re_added.status(), 200, "already watched");

    // Deleting removes; a bogus watch is refused loudly.
    let removed = client
        .delete(format!("{base}/v1/watches/github/acme/widgets"))
        .send()
        .await
        .expect("the delete goes out");
    assert_eq!(
        removed.json::<Value>().await.expect("body parses"),
        json!({"removed": true})
    );
    let bogus = client
        .post(format!("{base}/v1/watches"))
        .json(&json!({"forge": "gitea", "repo": "acme/barn"}))
        .send()
        .await
        .expect("the bogus watch posts");
    assert_eq!(bogus.status(), 400, "a hostless gitea watch is refused");

    // Gone from the table, the pass detects nothing.
    let report = daemon.tick().await.expect("the pass runs");
    assert_eq!(report.detected, 0, "the watch list reads fresh each pass");
}

#[tokio::test]
async fn the_watch_tools_mutate_the_same_table_the_http_routes_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // A second connection to the same db file — exactly what the
    // handler's watch tools get — lands in the table the routes read.
    let tools_store =
        Arc::new(rhema_claw::ClawStore::open(dir.path().join("dispatch.db")).expect("reopens"));
    let registry = watch_tools::registry(tools_store);
    let resolver = registry.resolver_for(&Allowlist::permissive());
    let ctx = rhema_core::ToolContext::default();
    let result = resolver
        .resolve(
            rhema_core::ToolCall {
                id: "c1".into(),
                name: "watch_repo".into(),
                args: json!({"forge": "github", "repo": "acme/gadgets", "token": "tok"}),
                provider_data: Value::Null,
            },
            &ctx,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);

    // HTTP sees the tool's row; the tool sees HTTP's rows.
    let listed: Value = client
        .get(format!("{base}/v1/watches"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses");
    assert_eq!(
        listed["watches"][0]["repo"], "acme/gadgets",
        "the tool's write is the HTTP surface's read: {listed}"
    );
    let tools_view = resolver
        .resolve(
            rhema_core::ToolCall {
                id: "c2".into(),
                name: "list_watches".into(),
                args: json!({}),
                provider_data: Value::Null,
            },
            &ctx,
        )
        .await;
    assert!(
        tools_view.content.contains("acme/gadgets"),
        "{}",
        tools_view.content
    );
    assert!(
        !tools_view.content.contains("tok"),
        "tool answers never carry tokens: {}",
        tools_view.content
    );

    // And unwatch_repo removes what watch_repo added.
    let removed = resolver
        .resolve(
            rhema_core::ToolCall {
                id: "c3".into(),
                name: "unwatch_repo".into(),
                args: json!({"forge": "github", "repo": "acme/gadgets"}),
                provider_data: Value::Null,
            },
            &ctx,
        )
        .await;
    assert!(!removed.is_error, "{}", removed.content);
    let listed: Value = client
        .get(format!("{base}/v1/watches"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses");
    assert_eq!(listed["watches"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn the_shim_answers_whole_and_streamed_and_ignores_incoming_history() {
    let provider = Arc::new(MockProvider::new([reply("on it"), reply("still here")]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider.clone()),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/chat/completions");

    // Non-stream: one completion object, the configured model echoed
    // when the request names none.
    let whole: Value = client
        .post(&url)
        .json(&json!({"messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .expect("the completion posts")
        .json()
        .await
        .expect("the completion parses");
    assert_eq!(whole["object"], "chat.completion");
    assert_eq!(whole["model"], "mock");
    assert_eq!(whole["choices"][0]["message"]["role"], "assistant");
    assert_eq!(whole["choices"][0]["message"]["content"], "on it");
    assert_eq!(whole["choices"][0]["finish_reason"], "stop");
    assert!(
        whole["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("chatcmpl-"))
    );
    assert!(whole["created"].as_u64().is_some());

    // Stream: chunks of the reply, the stop chunk, then [DONE].
    let streamed = client
        .post(&url)
        .json(&json!({
            "model": "custom/m",
            "stream": true,
            "messages": [{"role": "user", "content": "again"}],
        }))
        .send()
        .await
        .expect("the streamed completion posts");
    assert_eq!(streamed.status(), 200);
    let mut body = String::new();
    let mut stream = sse_body(streamed).await;
    read_until(&mut stream, &mut body, "data: [DONE]").await;
    assert!(
        body.contains(r#""delta":{"content":"still here"}"#),
        "the reply streamed as content chunks: {body}"
    );
    assert!(
        !body.contains(r#""delta":{"reasoning_content":"#),
        "a text-only mock emits no reasoning chunks: {body}"
    );
    assert!(
        body.contains(r#""finish_reason":"stop""#),
        "the final chunk stops: {body}"
    );
    assert!(
        body.contains(r#""model":"custom/m""#),
        "the model echoes: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}"); // The documented caveat, pinned: incoming history is ignored. Only
    // the last user message reached the agent.
    let store = SqliteStore::open(dir.path().join("dispatch.db")).expect("db reopens");
    let record = store
        .load(&ResourceKey::from("interactive:web"))
        .expect("the record loads")
        .expect("the key has a record");
    let stashed: Session =
        serde_json::from_value(record.session.expect("a session").core.driver_state)
            .expect("the stash parses");
    let prompts: Vec<&str> = stashed
        .history
        .iter()
        .filter_map(|message| match &message.content[0] {
            Content::Text(text) if message.role == Role::User => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["hello", "again"], "no transcript was replayed in");
}

#[tokio::test]
async fn the_shim_rejects_requests_without_a_user_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();
    let refused = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"messages": [{"role": "assistant", "content": "only me"}]}))
        .send()
        .await
        .expect("the request posts");
    assert_eq!(refused.status(), 400);
    let body: Value = refused.json().await.expect("the error parses");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("user")),
        "{body}"
    );
}

/// The bootstrap badge as the route serves it for `daemon`, with the
/// config's bearer.
async fn bootstrap_badge(daemon: &Daemon) -> Value {
    let base = serve(daemon).await;
    reqwest::Client::new()
        .get(format!("{base}/v1/bootstrap"))
        .header(reqwest::header::AUTHORIZATION, "Bearer sekrit")
        .send()
        .await
        .expect("the badge reads")
        .json()
        .await
        .expect("the badge parses")
}

/// GET /v1/bootstrap reflects the store live, behind the same bearer
/// check as every /v1 route: a UI badges first-run setup from it while
/// the onboarding offer is open.
#[tokio::test]
async fn the_bootstrap_route_reports_the_identity_files_and_requires_auth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, Some("sekrit")),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let url = format!("{base}/v1/bootstrap");
    let client = reqwest::Client::new();

    // The badge is a /v1 route: open reads are refused.
    assert_eq!(
        client
            .get(&url)
            .send()
            .await
            .expect("the open read goes out")
            .status(),
        401
    );

    // A fresh install: both files unwritten.
    assert_eq!(
        bootstrap_badge(&daemon).await,
        json!({"soul": false, "user": false})
    );

    // The files the onboarding flow writes flip the badge live — the
    // route reads the store per request, never a boot-time snapshot.
    let root = dir.path().join("bootstrap");
    std::fs::write(root.join("SOUL.md"), "I am claw: terse.").expect("write");
    assert_eq!(
        bootstrap_badge(&daemon).await,
        json!({"soul": true, "user": false})
    );
    std::fs::write(root.join("USER.md"), "The user is Ana.").expect("write");
    assert_eq!(
        bootstrap_badge(&daemon).await,
        json!({"soul": true, "user": true})
    );
}

#[tokio::test]
async fn a_configured_token_guards_v1_and_leaves_healthz_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, Some("sekrit")),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // Health is open; /v1 is not.
    assert_eq!(
        client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("health reads")
            .text()
            .await
            .expect("health body"),
        "ok"
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/watches"))
            .send()
            .await
            .expect("the unauthenticated read goes out")
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/watches"))
            .header(reqwest::header::AUTHORIZATION, "Bearer wrong")
            .send()
            .await
            .expect("the mis-authenticated read goes out")
            .status(),
        401
    );

    // The right bearer opens every /v1 route, chat included.
    let watches: Value = client
        .get(format!("{base}/v1/watches"))
        .header(reqwest::header::AUTHORIZATION, "Bearer sekrit")
        .send()
        .await
        .expect("the authenticated read goes out")
        .json()
        .await
        .expect("the list parses");
    assert_eq!(watches["watches"].as_array().map(Vec::len), Some(0));
    let whole: Value = client
        .post(format!("{base}/v1/chat/completions"))
        .header(reqwest::header::AUTHORIZATION, "Bearer sekrit")
        .json(&json!({"messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .expect("the completion posts")
        .json()
        .await
        .expect("the completion parses");
    assert_eq!(whole["choices"][0]["message"]["content"], "on it");
}

/// The watch list as the HTTP route serves it for `daemon`.
async fn list_watches(daemon: &Daemon) -> Value {
    let base = serve(daemon).await;
    reqwest::Client::new()
        .get(format!("{base}/v1/watches"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses")
}

/// The seed rule, through the daemon's assembly: an empty table takes
/// the config's watches; a populated table ignores the seed, so a
/// restart with a different YAML never rewrites what the table holds.
#[tokio::test]
async fn the_seed_imports_into_an_empty_table_and_then_the_table_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let yaml_seed = || ClawConfig {
        watches: vec![Watch {
            forge: ForgeKind::GitHub,
            repo: "acme/seeded".to_owned(),
            host: None,
            token: Some("tok".to_owned()),
        }],
        ..config(&dir, None)
    };
    let changed_seed = || ClawConfig {
        watches: vec![Watch {
            forge: ForgeKind::GitLab,
            repo: "group/other".to_owned(),
            host: None,
            token: Some("tok".to_owned()),
        }],
        ..config(&dir, None)
    };

    // First boot: the empty table takes the seed.
    let first = Daemon::build(
        &yaml_seed(),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the first daemon assembles");
    let listed = list_watches(&first).await;
    assert_eq!(listed["watches"][0]["repo"], "acme/seeded", "{listed}");

    // Second boot, different YAML, populated table: the table rules.
    let second = Daemon::build(
        &changed_seed(),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the second daemon assembles");
    let listed = list_watches(&second).await;
    assert_eq!(
        listed["watches"].as_array().map(Vec::len),
        Some(1),
        "the seed never rewrites a populated table: {listed}"
    );
    assert_eq!(listed["watches"][0]["repo"], "acme/seeded", "{listed}");
}

/// The ingest door: an external event lands on its key in the resident
/// dispatcher, an identical redelivery dedups, the daemon's own source
/// names cannot be re-registered, and the bearer guard covers it like
/// every `/v1` route.
#[tokio::test]
async fn ingest_feeds_the_resident_dispatcher_and_dedups_and_requires_auth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, Some("sekrit")),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/ingest");
    let auth = |request: reqwest::RequestBuilder| {
        request.header(reqwest::header::AUTHORIZATION, "Bearer sekrit")
    };
    let body = json!({"source": "phone", "key": "phone:ana", "payload": {"text": "hello"}});

    // The door is a /v1 route: open posts are refused.
    assert_eq!(
        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .expect("the open post goes out")
            .status(),
        401
    );

    let first: Value = auth(client.post(&url))
        .json(&body)
        .send()
        .await
        .expect("the event posts")
        .json()
        .await
        .expect("the answer parses");
    assert_eq!(first, json!({"ingested": "dispatched"}), "{first}");

    // The event sits on its key: the inventory — which reads the shared
    // store through the resident dispatcher — shows the depth.
    let sessions: Vec<Value> = auth(client.get(format!("{base}/v1/sessions")))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    let entry = sessions
        .iter()
        .find(|session| session["key"] == "phone:ana")
        .expect("the key is in the inventory");
    assert_eq!(entry["inbox_depth"], 1, "{entry}");
    assert_eq!(entry["state"], "idle");

    // An identical redelivery dedups; the depth does not move.
    let second: Value = auth(client.post(&url))
        .json(&body)
        .send()
        .await
        .expect("the replay posts")
        .json()
        .await
        .expect("the answer parses");
    assert_eq!(second, json!({"ingested": "duplicate"}), "{second}");
    let sessions: Vec<Value> = auth(client.get(format!("{base}/v1/sessions")))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    let entry = sessions
        .iter()
        .find(|session| session["key"] == "phone:ana")
        .expect("the key is still in the inventory");
    assert_eq!(entry["inbox_depth"], 1, "the dedup dropped it: {entry}");

    // A changed payload re-ingests: the depth moves.
    let changed: Value = auth(client.post(&url))
        .json(&json!({
            "source": "phone",
            "key": "phone:ana",
            "payload": {"text": "hello again"},
        }))
        .send()
        .await
        .expect("the changed post goes out")
        .json()
        .await
        .expect("the answer parses");
    assert_eq!(changed, json!({"ingested": "dispatched"}), "{changed}");

    // Shape checks: empty key, non-object payload, and the daemon's own
    // source names, which a re-register would misroute.
    for (bad, status) in [
        (json!({"source": "phone", "key": "  ", "payload": {}}), 400),
        (
            json!({"source": "phone", "key": "phone:ana", "payload": [1]}),
            422,
        ),
        (
            json!({"source": "github", "key": "phone:ana", "payload": {}}),
            400,
        ),
        (
            json!({"source": "tasks", "key": "phone:ana", "payload": {}}),
            400,
        ),
    ] {
        let response = auth(client.post(&url))
            .json(&bad)
            .send()
            .await
            .expect("the bad post goes out");
        assert_eq!(response.status(), status, "{bad}");
    }
}

/// The notification queue over HTTP: a second connection to the same db
/// file — exactly what the handler's notify tool gets — lands in the
/// queue the routes read; the poll returns a device's outstanding rows
/// ascending, `since` and `device` filter, and the ack delivers.
#[tokio::test]
async fn notifications_round_trip_over_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let store = Arc::new(
        rhema_claw::NotificationStore::open(dir.path().join("dispatch.db")).expect("reopens"),
    );
    let first = store.push("phone", "claw", "first").expect("push lands");
    let second = store.push("phone", "claw", "second").expect("push lands");
    store.push("watch", "claw", "wrist").expect("push lands");

    // The default queue is the phone's: two outstanding, ascending.
    let listed: Value = client
        .get(format!("{base}/v1/notifications"))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    let rows = listed["notifications"].as_array().expect("rows");
    assert_eq!(
        rows.iter()
            .map(|row| row["body"].as_str())
            .collect::<Vec<_>>(),
        [Some("first"), Some("second")],
        "{listed}"
    );
    assert_eq!(rows[0]["device"], "phone");
    assert!(rows[0]["delivered_at"].is_null(), "outstanding");

    // `since` skips what the device saw; `device` picks the queue.
    let since = first.id;
    let listed: Value = client
        .get(format!(
            "{base}/v1/notifications?device=phone&since={since}"
        ))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    assert_eq!(
        listed["notifications"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    let listed: Value = client
        .get(format!("{base}/v1/notifications?device=watch"))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    assert_eq!(listed["notifications"][0]["body"], "wrist", "{listed}");

    // Acking delivers: the row stops answering, a re-ack is a quiet
    // false, and an unknown id is refused the same quiet way.
    let acked: Value = client
        .post(format!("{base}/v1/notifications/{}/ack", second.id))
        .send()
        .await
        .expect("the ack posts")
        .json()
        .await
        .expect("the ack parses");
    assert_eq!(acked, json!({"acked": true}), "{acked}");
    let listed: Value = client
        .get(format!("{base}/v1/notifications"))
        .send()
        .await
        .expect("the poll reads")
        .json()
        .await
        .expect("the poll parses");
    assert_eq!(
        listed["notifications"].as_array().map(Vec::len),
        Some(1),
        "the acked row is delivered: {listed}"
    );
    for id in [second.id, 10_000] {
        let acked: Value = client
            .post(format!("{base}/v1/notifications/{id}/ack"))
            .send()
            .await
            .expect("the ack posts")
            .json()
            .await
            .expect("the ack parses");
        assert_eq!(acked, json!({"acked": false}), "{acked}");
    }
}

/// A registry whose only backend is the scripted mock.
fn mock_registry(provider: Arc<dyn rhema_core::ModelProvider>) -> Arc<rhema_claw::BackendRegistry> {
    Arc::new(
        rhema_claw::BackendRegistry::new(
            vec![rhema_claw::Backend::headless("mock", provider)],
            None,
        )
        .expect("a single mock backend always constructs"),
    )
}

/// The providers surface: the registry's list, the active pair, a
/// runtime switch, and the switch's persistence across a daemon
/// rebuild — the whole point of the runtime registry.
#[tokio::test]
async fn providers_are_listed_switched_and_persisted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let listed: Value = client
        .get(format!("{base}/v1/providers"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses");
    assert_eq!(listed["active"]["provider"], "mock");
    assert_eq!(listed["active"]["model"], "mock");
    let kinds: Vec<&str> = listed["providers"]
        .as_array()
        .expect("the providers array")
        .iter()
        .filter_map(|p| p["kind"].as_str())
        .collect();
    assert_eq!(kinds, vec!["mock"]);

    // A switch to a model within the same backend.
    let switched: Value = client
        .put(format!("{base}/v1/providers/active"))
        .json(&json!({"provider": "mock", "model": "mock-large"}))
        .send()
        .await
        .expect("the switch posts")
        .json()
        .await
        .expect("the switch parses");
    assert_eq!(switched["active"]["model"], "mock-large");

    // An unknown kind is a 400, and the active pair is untouched.
    let refused = client
        .put(format!("{base}/v1/providers/active"))
        .json(&json!({"provider": "nope"}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(refused.status(), 400);

    // The live-listing route: the mock backend has none, so 404 —
    // the shape, not the network.
    let models = client
        .get(format!("{base}/v1/providers/mock/models"))
        .send()
        .await
        .expect("the models route answers");
    assert_eq!(models.status(), 404);

    // Persistence: a rebuilt daemon over the same db reopens the
    // switched pair, not the config seed.
    drop(base);
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon reassembles");
    assert_eq!(daemon.backends().active().1, "mock-large");
}

/// The credentials surface: a runtime login adds a backend, the list
/// names where its credential came from, a re-add replaces it in place,
/// and a removal drops it — refusing to remove the last backend, and
/// moving the active pair when the removed kind was serving.
#[tokio::test]
async fn providers_gain_and_lose_backends_over_the_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // A login: llama.cpp takes a url, no key, and joins the registry.
    let added: Value = client
        .put(format!("{base}/v1/providers/llama-cpp/credentials"))
        .json(&json!({"url": "http://127.0.0.1:9999"}))
        .send()
        .await
        .expect("the login posts")
        .json()
        .await
        .expect("the state parses");
    let row = added["providers"]
        .as_array()
        .expect("the providers array")
        .iter()
        .find(|p| p["kind"] == "llama-cpp")
        .expect("the new backend listed")
        .clone();
    assert_eq!(row["source"], "stored");
    assert_eq!(row["default_model"], "local");
    assert_eq!(added["active"]["provider"], "mock", "the pair is untouched");

    // A re-add replaces the kind in place instead of duplicating it.
    let replaced: Value = client
        .put(format!("{base}/v1/providers/llama-cpp/credentials"))
        .json(&json!({"url": "http://127.0.0.1:10000"}))
        .send()
        .await
        .expect("the re-add posts")
        .json()
        .await
        .expect("the state parses");
    let llama_rows = replaced["providers"]
        .as_array()
        .expect("the providers array")
        .iter()
        .filter(|p| p["kind"] == "llama-cpp")
        .count();
    assert_eq!(llama_rows, 1);

    // A keyed kind without its key is a 400, and nothing was added.
    let refused = client
        .put(format!("{base}/v1/providers/google/credentials"))
        .json(&json!({}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(refused.status(), 400);
    let listed: Value = client
        .get(format!("{base}/v1/providers"))
        .send()
        .await
        .expect("the list reads")
        .json()
        .await
        .expect("the list parses");
    assert!(
        !listed["providers"]
            .as_array()
            .expect("the providers array")
            .iter()
            .any(|p| p["kind"] == "google")
    );

    // An unknown kind is a 400 too.
    let unknown = client
        .put(format!("{base}/v1/providers/nope/credentials"))
        .json(&json!({"api_key": "k"}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(unknown.status(), 400);

    // Removal: the active kind going away moves the pair to what
    // remains, and the persisted pair moves with it.
    let removed: Value = client
        .delete(format!("{base}/v1/providers/mock"))
        .send()
        .await
        .expect("the removal posts")
        .json()
        .await
        .expect("the state parses");
    assert_eq!(removed["active"]["provider"], "llama-cpp");

    // The last backend refuses to go.
    let last = client
        .delete(format!("{base}/v1/providers/llama-cpp"))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(last.status(), 400);

    // A removal of an unknown kind is a 400 as well.
    let gone = client
        .delete(format!("{base}/v1/providers/ethereal"))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(gone.status(), 400);
}

/// The agent identity surface: the text fields round-trip with
/// partial-save semantics (an absent field is left alone, an empty one
/// clears), the avatar uploads and serves back with its content type,
/// and clearing it 404s the read.
#[tokio::test]
async fn the_agent_identity_is_editable_over_the_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let empty: Value = client
        .get(format!("{base}/v1/agent"))
        .send()
        .await
        .expect("the identity reads")
        .json()
        .await
        .expect("the identity parses");
    assert_eq!(empty["avatar"], false);
    assert_eq!(empty["logo"], false);

    // A save names the fields it touches.
    let saved: Value = client
        .put(format!("{base}/v1/agent"))
        .json(&json!({"name": "Kosmos", "mark": "✦", "tagline": "order"}))
        .send()
        .await
        .expect("the save posts")
        .json()
        .await
        .expect("the save parses");
    assert_eq!(saved["name"], "Kosmos");
    assert_eq!(saved["mark"], "✦");
    assert_eq!(saved["avatar"], false);

    // A partial save leaves the fields it omitted.
    let partial: Value = client
        .put(format!("{base}/v1/agent"))
        .json(&json!({"name": "Kosmos Prime"}))
        .send()
        .await
        .expect("the partial save posts")
        .json()
        .await
        .expect("the partial save parses");
    assert_eq!(partial["name"], "Kosmos Prime");
    assert_eq!(partial["mark"], "✦", "untouched by the partial save");

    // An empty string clears its field.
    let cleared: Value = client
        .put(format!("{base}/v1/agent"))
        .json(&json!({"mark": ""}))
        .send()
        .await
        .expect("the clear posts")
        .json()
        .await
        .expect("the clear parses");
    assert_eq!(cleared["mark"], serde_json::Value::Null);

    // An overlong field is a 400.
    let refused = client
        .put(format!("{base}/v1/agent"))
        .json(&json!({"name": "x".repeat(65)}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(refused.status(), 400);

    // The avatar: upload, serve, clear.
    let uploaded: Value = client
        .put(format!("{base}/v1/agent/avatar"))
        .header("content-type", "image/png")
        .body(vec![1, 2, 3, 4])
        .send()
        .await
        .expect("the upload posts")
        .json()
        .await
        .expect("the upload parses");
    assert_eq!(uploaded["avatar"], true);

    let served = client
        .get(format!("{base}/v1/agent/avatar"))
        .send()
        .await
        .expect("the image reads");
    assert_eq!(served.status(), 200);
    assert_eq!(
        served
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        served.bytes().await.expect("the bytes read").as_ref(),
        &[1, 2, 3, 4]
    );

    // A non-image upload is a 400.
    let not_image = client
        .put(format!("{base}/v1/agent/logo"))
        .header("content-type", "text/html")
        .body("<b>no</b>")
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(not_image.status(), 400);

    let cleared_image: Value = client
        .delete(format!("{base}/v1/agent/avatar"))
        .send()
        .await
        .expect("the clear posts")
        .json()
        .await
        .expect("the clear parses");
    assert_eq!(cleared_image["avatar"], false);
    let gone = client
        .get(format!("{base}/v1/agent/avatar"))
        .send()
        .await
        .expect("the read answers");
    assert_eq!(gone.status(), 404);
}
