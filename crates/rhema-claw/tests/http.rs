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

/// The scripted assistant turn that asks for a tool by name.
fn tool_call(id: &str, name: &str, args: Value) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(rhema_core::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                args,
                provider_data: Value::Null,
            })],
        },
        stop_reason: StopReason::ToolUse,
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
        max_turn_steps: rhema_claw::config::DEFAULT_MAX_TURN_STEPS,
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
        let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(_) => panic!(
                "the stream ended early waiting for {needle:?}; seen: {}",
                &seen[seen.len().saturating_sub(600)..]
            ),
            Err(_) => panic!(
                "timed out waiting for {needle:?}; seen tail: {}",
                &seen[seen.len().saturating_sub(600)..]
            ),
        };
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

/// A turn nobody watched is still replayable: a client arriving after
/// the fact — a reloaded browser, a reopened phone — subscribes cold and
/// gets the finished turns back before the live feed. The same holds
/// across a daemon restart: the flushed record seeds the next boot's
/// hub.
#[tokio::test]
async fn a_cold_subscription_replays_the_turns_it_missed_even_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(&dir, None);
    {
        let daemon = Daemon::build(
            &config,
            mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
            rhema_rsd::DispatcherConfig::default(),
        )
        .expect("the daemon assembles");
        let base = serve(&daemon).await;
        let client = reqwest::Client::new();

        // The turn runs with nobody watching. The POST's drain finishes
        // it and flushes the record before the 202 returns.
        let accepted = client
            .post(format!("{base}/v1/sessions/interactive:web/messages"))
            .json(&json!({"text": "hello"}))
            .send()
            .await
            .expect("the message posts");
        assert_eq!(accepted.status(), 202);
    }

    // A fresh daemon over the same database — the restart — serves the
    // same transcript to the first subscriber.
    let daemon = Daemon::build(
        &config,
        mock_registry(Arc::new(MockProvider::new([]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the restarted daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();
    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    assert_eq!(events.status(), 200);
    let mut stream = sse_body(events).await;
    let mut seen = String::new();
    read_until(
        &mut stream,
        &mut seen,
        r#""type":"user_message","text":"hello""#,
    )
    .await;
    read_until(
        &mut stream,
        &mut seen,
        r#""type":"text_delta","text":"on it""#,
    )
    .await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    assert!(
        seen.find(r#""type":"user_message""#).unwrap()
            < seen.find(r#""type":"text_delta""#).unwrap(),
        "the replay arrives in stream order: {seen}"
    );
}

/// The `since` cursor: a client that already holds the record's first
/// event receives the record minus it — the phone client's transcript
/// cache, so reopening a session is not a rebuild from zero.
#[tokio::test]
async fn a_since_cursor_skips_the_replays_oldest_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(&dir, None);
    {
        let daemon = Daemon::build(
            &config,
            mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
            rhema_rsd::DispatcherConfig::default(),
        )
        .expect("the daemon assembles");
        let base = serve(&daemon).await;
        let client = reqwest::Client::new();

        // One turn, nobody watching; the drain flushes the record
        // before the 202 returns.
        let accepted = client
            .post(format!("{base}/v1/sessions/interactive:web/messages"))
            .json(&json!({"text": "hello"}))
            .send()
            .await
            .expect("the message posts");
        assert_eq!(accepted.status(), 202);
    }

    let daemon = Daemon::build(
        &config,
        mock_registry(Arc::new(MockProvider::new([]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the restarted daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();
    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events?since=1"))
        .send()
        .await
        .expect("the events request goes out");
    assert_eq!(events.status(), 200);
    let mut stream = sse_body(events).await;
    let mut seen = String::new();
    read_until(&mut stream, &mut seen, r#""type":"text_delta""#).await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    assert!(
        !seen.contains(r#""type":"user_message""#),
        "the event the cursor names does not replay: {seen}"
    );
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

    // The addable kinds name their login fields with what the daemon
    // demands: keyed kinds cannot construct without an api_key, and
    // llama.cpp serves keyless on a default url, so nothing gates it.
    let addable = listed["addable"].as_array().expect("the addable array");
    let fields_of = |kind: &str| {
        addable
            .iter()
            .find(|entry| entry["kind"] == kind)
            .expect("the kind is addable")["fields"]
            .clone()
    };
    assert_eq!(
        fields_of("google"),
        json!([{ "name": "api_key", "required": true }])
    );
    assert_eq!(
        fields_of("llama-cpp"),
        json!([
            { "name": "url", "required": false },
            { "name": "api_key", "required": false }
        ])
    );

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

    // A login: llama.cpp takes a url, no key, and joins the registry
    // under its kind's id.
    let added: Value = client
        .put(format!("{base}/v1/providers/llama-cpp/credentials"))
        .json(&json!({"kind": "llama-cpp", "url": "http://127.0.0.1:9999"}))
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
    assert_eq!(row["id"], "llama-cpp");
    assert_eq!(row["source"], "stored");
    assert_eq!(row["default_model"], "local");
    assert_eq!(added["active"]["provider"], "mock", "the pair is untouched");

    // A re-add replaces the id in place instead of duplicating it.
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
        .json(&json!({"kind": "google"}))
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

    // An unknown kind is a 400 too, with or without the key.
    for body in [
        json!({"api_key": "k"}),
        json!({"kind": "nope", "api_key": "k"}),
    ] {
        let unknown = client
            .put(format!("{base}/v1/providers/nope/credentials"))
            .json(&body)
            .send()
            .await
            .expect("the refusal posts");
        assert_eq!(unknown.status(), 400, "{body}");
    }

    // Removal: the active instance going away moves the pair to what
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

/// The registry a rebuilt daemon boots over the same db: discovery's
/// stored instances plus the scripted mock, so a persisted active pair
/// referencing a named instance restores against it.
fn discovered_registry(dir: &tempfile::TempDir) -> Arc<rhema_claw::BackendRegistry> {
    let cfg = config(dir, None);
    let profile = rhema_config::Profile::default();
    let stored_rows = || {
        rhema_claw::SettingsStore::open(dir.path().join("dispatch.db"))
            .ok()
            .and_then(|store| {
                store
                    .get_prefix(rhema_claw::backends::PROVIDER_KEY_PREFIX)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, json)| {
                let id = key.strip_prefix(rhema_claw::backends::PROVIDER_KEY_PREFIX)?;
                serde_json::from_str(&json).ok().map(|p| (id.to_owned(), p))
            })
            .collect::<Vec<_>>()
    };
    let mut backends =
        rhema_claw::backends::discover(&cfg, &profile, &|_| None, &stored_rows, None);
    backends.push(rhema_claw::Backend::headless(
        "mock",
        Arc::new(MockProvider::new([reply("on it")])),
    ));
    Arc::new(
        rhema_claw::BackendRegistry::new(backends, None)
            .expect("the discovered registry always constructs"),
    )
}

/// Named instances: two logins of one kind coexist under two ids, an
/// id replaces in place (kind optional), kind mismatches and kindless
/// new ids and bad names are refused, and the active pair pointing at
/// a named instance survives a daemon rebuild over the same store.
#[tokio::test]
async fn named_provider_instances_share_the_surface() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new([reply("on it")]))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // Two llama.cpp servers under two names, two urls.
    for (name, url) in [
        ("local-a", "http://127.0.0.1:9001"),
        ("local-b", "http://127.0.0.1:9002"),
    ] {
        let added: Value = client
            .put(format!("{base}/v1/providers/{name}/credentials"))
            .json(&json!({"kind": "llama-cpp", "url": url}))
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
            .find(|p| p["id"] == name)
            .unwrap_or_else(|| panic!("{name} listed: {added}"));
        assert_eq!(row["kind"], "llama-cpp", "{row}");
        assert_eq!(row["source"], "stored", "{row}");
    }

    // Replace in place by id, kind omitted: still one local-a, still a
    // llama.cpp.
    let replaced: Value = client
        .put(format!("{base}/v1/providers/local-a/credentials"))
        .json(&json!({"url": "http://127.0.0.1:9003"}))
        .send()
        .await
        .expect("the re-add posts")
        .json()
        .await
        .expect("the state parses");
    let rows = replaced["providers"]
        .as_array()
        .expect("the providers array");
    assert_eq!(rows.iter().filter(|p| p["id"] == "local-a").count(), 1);
    assert_eq!(
        rows.iter().filter(|p| p["kind"] == "llama-cpp").count(),
        2,
        "both named instances still serve: {rows:?}"
    );

    // A kind mismatch on replace is a 400.
    let mismatched = client
        .put(format!("{base}/v1/providers/local-a/credentials"))
        .json(&json!({"kind": "google", "api_key": "k"}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(mismatched.status(), 400);

    // A new id without a kind is a 400: the first login must say what
    // it is constructing.
    let kindless = client
        .put(format!("{base}/v1/providers/newone/credentials"))
        .json(&json!({"api_key": "k"}))
        .send()
        .await
        .expect("the refusal posts");
    assert_eq!(kindless.status(), 400);

    // Bad names are refused: whitespace, a slash, over the length cap.
    let long = "x".repeat(65);
    for name in ["bad%20name", "a%2Fb", long.as_str()] {
        let refused = client
            .put(format!("{base}/v1/providers/{name}/credentials"))
            .json(&json!({"kind": "llama-cpp"}))
            .send()
            .await
            .expect("the refusal posts");
        assert_eq!(refused.status(), 400, "{name}");
    }

    // The active pair moves to a named instance and persists.
    let switched: Value = client
        .put(format!("{base}/v1/providers/active"))
        .json(&json!({"provider": "local-b"}))
        .send()
        .await
        .expect("the switch posts")
        .json()
        .await
        .expect("the switch parses");
    assert_eq!(switched["active"]["provider"], "local-b");

    drop(base);
    let daemon = Daemon::build(
        &config(&dir, None),
        discovered_registry(&dir),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon reassembles");
    assert_eq!(
        daemon.backends().active().0,
        "local-b",
        "the named instance is still active after the rebuild"
    );
}

/// The consent round-trip, over the wire: a turn asks through
/// `ask_secret`, the message POST that triggered it holds until the
/// answer lands, the pending list shows the ask, and the secret ends up
/// in the accounts — never in the transcript.
#[tokio::test]
async fn a_secret_ask_is_answered_over_the_consent_routes() {
    let provider = Arc::new(MockProvider::new([
        tool_call(
            "c1",
            "ask_secret",
            json!({
                "service": "github",
                "username": "octocat",
                "reason": "to file the issue",
            }),
        ),
        reply("stored, thanks."),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();

    // The turn blocks inside the tool: the POST returns only when the
    // human answers, so it runs on its own task.
    let poster = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/v1/sessions/interactive:web/messages"))
                .json(&json!({"text": "file the issue"}))
                .send()
                .await
                .expect("the message posts")
        }
    });

    // The ask appears on the pending list, then on the stream.
    let consents: Value = loop {
        let consents: Value = client
            .get(format!("{base}/v1/consents"))
            .send()
            .await
            .expect("consents reads")
            .json()
            .await
            .expect("consents parses");
        if consents["consents"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
        {
            break consents;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let id = consents["consents"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    assert_eq!(consents["consents"][0]["kind"], "secret");
    read_until(&mut stream, &mut seen, r#""type":"consent_request""#).await;

    // A wrong-shaped answer is refused and the ask stays open.
    let refused = client
        .post(format!("{base}/v1/consents/{id}"))
        .json(&json!({}))
        .send()
        .await
        .expect("the answer posts");
    assert_eq!(refused.status(), 422);

    // The answer: the secret goes in, the POST unblocks, the account
    // exists redacted.
    let answered = client
        .post(format!("{base}/v1/consents/{id}"))
        .json(&json!({"secret": "gh_pat_123"}))
        .send()
        .await
        .expect("the answer posts");
    assert_eq!(answered.status(), 200);
    let settled: Value = answered.json().await.expect("the answer parses");
    assert_eq!(settled["granted"], true);
    let account_id = settled["account_id"]
        .as_str()
        .expect("an account id")
        .to_owned();

    let posted = poster.await.expect("the post joins");
    assert_eq!(posted.status(), 202);

    let accounts: Value = client
        .get(format!("{base}/v1/accounts"))
        .send()
        .await
        .expect("accounts reads")
        .json()
        .await
        .expect("accounts parses");
    assert_eq!(accounts["accounts"].as_array().expect("a list").len(), 1);
    assert_eq!(accounts["accounts"][0]["service"], "github");
    assert_eq!(accounts["accounts"][0]["id"], account_id);
    assert!(
        !seen.contains("gh_pat_123"),
        "the secret never reaches the transcript: {seen}"
    );
    read_until(&mut stream, &mut seen, r#""type":"consent_resolved""#).await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;

    // A second answer is a conflict, and the ask has left the list.
    let again = client
        .post(format!("{base}/v1/consents/{id}"))
        .json(&json!({"secret": "x"}))
        .send()
        .await
        .expect("the answer posts");
    assert_eq!(again.status(), 409);
    let consents: Value = client
        .get(format!("{base}/v1/consents"))
        .send()
        .await
        .expect("consents reads")
        .json()
        .await
        .expect("consents parses");
    assert!(consents["consents"].as_array().expect("a list").is_empty());
}

/// The attachment round-trip, over the wire: a turn calls
/// `attach_file`, the stream carries the card, and the bytes are
/// fetchable at the card's address with the content type its name
/// implies.
#[tokio::test]
async fn an_attached_file_is_fetchable_over_its_route() {
    let workspace = |dir: &tempfile::TempDir| {
        let workspace = dir.path().join("workspaces").join("interactive");
        std::fs::create_dir_all(&workspace).expect("workspace");
        workspace
    };
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workspace(&dir).join("notes.md"),
        "# notes\nthe thing broke\n",
    )
    .expect("the file writes");
    let provider = Arc::new(MockProvider::new([
        tool_call(
            "c1",
            "attach_file",
            json!({"path": "notes.md", "caption": "look"}),
        ),
        reply("attached."),
    ]));
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();

    let accepted = client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "hand me the notes"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202);

    read_until(&mut stream, &mut seen, r#""type":"attachment""#).await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    let marker = seen
        .split(r#""type":"attachment""#)
        .nth(1)
        .and_then(|rest| rest.split(r#""id":""#).nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("the card names its id")
        .to_owned();

    let fetched = client
        .get(format!("{base}/v1/attachments/{marker}"))
        .send()
        .await
        .expect("the fetch goes out");
    assert_eq!(
        fetched.status(),
        200,
        "marker {marker:?}; tail {:?}",
        &seen[seen.len().saturating_sub(300)..]
    );
    assert_eq!(
        fetched
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8"),
        "the content type follows the name"
    );
    assert_eq!(
        fetched.bytes().await.expect("the bytes arrive"),
        "# notes\nthe thing broke\n"
    );

    let missing = client
        .get(format!("{base}/v1/attachments/att-nowhere"))
        .send()
        .await
        .expect("the fetch goes out");
    assert_eq!(missing.status(), 404);
}

/// The phone control loop, over the wire: a turn acts through the
/// `phone` tool, the consent card is answered with a grant, the command
/// lands on the device's poll list, and the device's report is what the
/// tool finally answers with.
#[tokio::test]
async fn a_phone_control_runs_consent_queue_and_report() {
    let provider = Arc::new(MockProvider::new([
        tool_call(
            "c1",
            "phone",
            json!({"action": "dnd_on", "device": "pixel", "reason": "the movie"}),
        ),
        reply("done."),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();

    let poster = tokio::spawn({
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/v1/sessions/interactive:web/messages"))
                .json(&json!({"text": "quiet the phone"}))
                .send()
                .await
                .expect("the message posts")
        }
    });

    // The consent card; the human grants it.
    let consents: Value = loop {
        let consents: Value = client
            .get(format!("{base}/v1/consents"))
            .send()
            .await
            .expect("consents reads")
            .json()
            .await
            .expect("consents parses");
        if consents["consents"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
        {
            break consents;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(consents["consents"][0]["kind"], "phone");
    assert_eq!(
        consents["consents"][0]["summary"], "phone dnd_on on pixel",
        "{}",
        consents
    );
    let id = consents["consents"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    read_until(&mut stream, &mut seen, r#""type":"consent_request""#).await;
    let granted = client
        .post(format!("{base}/v1/consents/{id}"))
        .json(&json!({"grant": true}))
        .send()
        .await
        .expect("the grant posts");
    assert_eq!(granted.status(), 200);

    // The command joins the device's poll list; the device executes and
    // reports.
    let commands: Value = loop {
        let commands: Value = client
            .get(format!("{base}/v1/phone/commands?device=pixel"))
            .send()
            .await
            .expect("commands reads")
            .json()
            .await
            .expect("commands parses");
        if commands["commands"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
        {
            break commands;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(commands["commands"][0]["action"], "dnd_on");
    let command_id = commands["commands"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let reported = client
        .post(format!("{base}/v1/phone/commands/{command_id}/result"))
        .json(&json!({"ok": true, "detail": "dnd is on"}))
        .send()
        .await
        .expect("the report posts");
    assert_eq!(reported.status(), 200);

    let posted = poster.await.expect("the post joins");
    assert_eq!(posted.status(), 202);
    read_until(&mut stream, &mut seen, r#""type":"consent_resolved""#).await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;

    // A redelivered report is a conflict, and the poll list is empty.
    let again = client
        .post(format!("{base}/v1/phone/commands/{command_id}/result"))
        .json(&json!({"ok": true}))
        .send()
        .await
        .expect("the report posts");
    assert_eq!(again.status(), 409);
    let commands: Value = client
        .get(format!("{base}/v1/phone/commands?device=pixel"))
        .send()
        .await
        .expect("commands reads")
        .json()
        .await
        .expect("commands parses");
    assert!(commands["commands"].as_array().expect("a list").is_empty());
}

/// The phone's own status reports feed the state the `phone` tool
/// reads: an ingest to a `phone:<device>:<topic>` key is conversation
/// and observation at once.
#[tokio::test]
async fn an_ingested_phone_report_feeds_the_status_the_tool_reads() {
    let provider = Arc::new(MockProvider::new([
        tool_call(
            "c1",
            "phone",
            json!({"action": "status", "device": "pixel"}),
        ),
        reply("battery is fine."),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // The report the PhoneStatusWorker's shape makes: a status event on
    // its own key.
    let ingested = client
        .post(format!("{base}/v1/ingest"))
        .json(&json!({
            "source": "phone",
            "key": "phone:pixel:battery",
            "payload": {"topic": "battery", "value": "64", "charging": false},
        }))
        .send()
        .await
        .expect("the report posts");
    assert_eq!(ingested.status(), 202);

    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();
    let accepted = client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "how is the phone?"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202);
    read_until(&mut stream, &mut seen, r#""type":"tool_result""#).await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    // The summary is bounded, so the full payload never rides the
    // stream — but the observed-true verdict and the device do.
    assert!(
        seen.contains(r#""summary":"{\"observed\":true,\"snapshot\":{\"device\":\"pixel\""#),
        "the tool read the ingested observation: {seen}"
    );
    assert!(
        !seen.contains(r#""type":"consent_request""#),
        "reading never asks: {seen}"
    );
}

/// The fleet routes: reports register devices, the list carries the
/// self-reported label and every topic's latest observation, a
/// state-only topic ("observed") never joins the conversation, and a
/// forget drops the device — answering `forgotten: false` on an
/// already-gone name instead of a 404.
#[tokio::test]
async fn the_phone_fleet_lists_labels_and_forgets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(Arc::new(MockProvider::new(Vec::new()))),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    async fn report(client: &reqwest::Client, base: &str, key: &str, payload: Value) -> Value {
        client
            .post(format!("{base}/v1/ingest"))
            .json(&json!({"source": "phone", "key": key, "payload": payload}))
            .send()
            .await
            .expect("the report posts")
            .json::<Value>()
            .await
            .expect("the answer parses")
    }

    let battery = report(
        &client,
        &base,
        "phone:pixel-1:battery",
        json!({"topic": "battery", "value": "64", "charging": false}),
    )
    .await;
    assert_eq!(battery, json!({"ingested": "observed"}), "{battery}");
    let location = report(
        &client,
        &base,
        "phone:pixel-1:location",
        json!({"topic": "location", "lat": 48.85, "lon": 2.35, "accuracy_m": 12.0}),
    )
    .await;
    assert_eq!(location, json!({"ingested": "observed"}), "{location}");
    report(
        &client,
        &base,
        "phone:pixel-1:device",
        json!({"topic": "device", "manufacturer": "Google", "model": "Pixel 9"}),
    )
    .await;
    report(
        &client,
        &base,
        "phone:pixel-2:battery",
        json!({"topic": "battery", "value": "10", "charging": true}),
    )
    .await;

    // Reports are observations, not conversation: no report leaves a
    // session key behind, whichever topic it carries.
    let sessions: Vec<Value> = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    let keys = sessions
        .iter()
        .map(|session| session["key"].as_str().expect("a key"))
        .collect::<Vec<_>>();
    assert!(
        keys.iter().all(|key| !key.starts_with("phone:")),
        "a report never dispatches: {keys:?}"
    );

    let fleet: Value = client
        .get(format!("{base}/v1/phones"))
        .send()
        .await
        .expect("the fleet reads")
        .json()
        .await
        .expect("the fleet parses");
    let phones = fleet["phones"].as_array().expect("a list");
    assert_eq!(phones.len(), 2, "{fleet}");
    assert_eq!(phones[0]["device"], "pixel-1");
    assert_eq!(phones[0]["label"], "Google Pixel 9");
    assert_eq!(phones[0]["topics"]["battery"]["value"], "64");
    assert_eq!(phones[0]["topics"]["location"]["lat"], 48.85);
    assert_eq!(phones[1]["device"], "pixel-2");
    assert!(
        phones[1]["label"].is_null(),
        "no device topic, no label: {fleet}"
    );

    let forgotten: Value = client
        .delete(format!("{base}/v1/phones/pixel-1"))
        .send()
        .await
        .expect("the forget goes out")
        .json()
        .await
        .expect("the answer parses");
    assert_eq!(forgotten, json!({"forgotten": true}), "{forgotten}");
    let fleet: Value = client
        .get(format!("{base}/v1/phones"))
        .send()
        .await
        .expect("the fleet reads")
        .json()
        .await
        .expect("the fleet parses");
    assert_eq!(fleet["phones"].as_array().expect("a list").len(), 1);
    assert_eq!(fleet["phones"][0]["device"], "pixel-2");

    // Forgetting an already-gone name is data, not an error.
    let again: Value = client
        .delete(format!("{base}/v1/phones/pixel-1"))
        .send()
        .await
        .expect("the forget goes out")
        .json()
        .await
        .expect("the answer parses");
    assert_eq!(again, json!({"forgotten": false}), "{again}");
}

/// Creating a session over HTTP: an empty `interactive:` key exists
/// before its first message, a second create of the same name is a
/// conflict, and a name that would not read as a key is refused.
#[tokio::test]
async fn a_session_is_creatable_over_http() {
    let provider = Arc::new(MockProvider::new([reply("on it")]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let created = client
        .post(format!("{base}/v1/sessions"))
        .json(&json!({"name": "my-notes"}))
        .send()
        .await
        .expect("the create posts");
    assert_eq!(created.status(), 201);
    assert_eq!(
        created.json::<Value>().await.expect("body parses"),
        json!({"key": "interactive:my-notes", "created": true})
    );

    // The session exists idle, before any message — and a message to it
    // turns it, like any other resident key.
    let sessions: Value = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    let keys: Vec<&str> = sessions
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|session| session["key"].as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["interactive:my-notes"],
        "the created key is listed"
    );

    let again = client
        .post(format!("{base}/v1/sessions"))
        .json(&json!({"name": "my-notes"}))
        .send()
        .await
        .expect("the create posts");
    assert_eq!(again.status(), 409);

    for bad in ["", "has space", "sl/ash", "a".repeat(65).as_str()] {
        let refused = client
            .post(format!("{base}/v1/sessions"))
            .json(&json!({"name": bad}))
            .send()
            .await
            .expect("the create posts");
        assert_eq!(refused.status(), 400, "refusing {bad:?}");
    }
}

/// The wake-itself loop, over the wire: the model schedules a turn into
/// the chat session it is talking in, the daemon's detect pass fires it
/// when due, and the reply lands in that same session — one session,
/// one context, on both sides of the sleep.
#[tokio::test]
async fn a_scheduled_task_wakes_the_chat_session_it_names() {
    let provider = Arc::new(MockProvider::new([
        tool_call(
            "c1",
            "schedule_task",
            json!({
                "prompt": "summarize tonight's work",
                "at": 1_791_734_400_000_i64,
                "session": "interactive:web",
            }),
        ),
        reply("scheduled for tomorrow morning."),
        // The wake's turn.
        reply("tonight you shipped the claw."),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();

    let accepted = client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "wake me here tomorrow morning with a summary"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202);
    read_until(&mut stream, &mut seen, r#""type":"tool_result""#).await;
    read_until(&mut stream, &mut seen, "scheduled for tomorrow morning.").await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;

    // The scheduled instant is tomorrow, far past the test's patience:
    // pull it due by hand (the row is the store's, the mechanism is the
    // scheduler's) and run one daemon pass — detect, admit, run.
    {
        use rhema_claw::task_store::TaskStore;
        let store = TaskStore::open(dir.path().join("dispatch.db")).expect("the task table opens");
        let task = &store.list().expect("the row reads")[0];
        store.rearm(task.id, 1).expect("pulled due");
    }
    let report = daemon.tick().await.expect("the tick runs");
    assert_eq!(report.detected, 1, "the wake was detected");

    // The wake fired in the chat session: its prompt is the next user
    // message and the turn's reply streamed after it.
    read_until(&mut stream, &mut seen, "summarize tonight's work").await;
    read_until(&mut stream, &mut seen, "tonight you shipped the claw.").await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
    assert_eq!(
        seen.matches(r#""type":"user_message""#).count(),
        2,
        "the chat's ask, then the wake: {seen}"
    );
    // The session notified nothing: interactive replies stay in the chat.
    let notifications: Value = client
        .get(format!("{base}/v1/notifications"))
        .send()
        .await
        .expect("notifications read")
        .json()
        .await
        .expect("notifications parse");
    assert!(
        notifications["notifications"]
            .as_array()
            .expect("a list")
            .is_empty(),
        "{notifications}"
    );
}

/// The durability story, over the wire: a daemon restart cut a turn
/// mid-flight, leaving the dead process's lease on the key. At boot the
/// daemon releases it outright — single-node means the only lease
/// holder was the process that died — so the key answers its next
/// message instead of stalling behind a ghost for the dead lease's
/// remaining TTL.
#[tokio::test]
async fn a_restart_releases_a_dead_lease_and_the_key_answers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("dispatch.db");
    {
        // The old process, mid-turn: it holds a fresh lease on the chat
        // session — hours left, the turn never released it.
        use rhema_rsd::DispatcherConfig;
        let store = Arc::new(SqliteStore::open(&db).expect("the store opens"));
        let dispatcher = rhema_rsd::Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                lease_ttl_millis: 3_600_000,
                ..DispatcherConfig::default()
            },
            Arc::new(rhema_rsd::SystemClock),
            store,
        );
        let (lease, _) = dispatcher
            .acquire(ResourceKey::from("interactive:web"), "the-old-process")
            .expect("the old process takes its lease");
        assert_eq!(lease.holder, "the-old-process");
        // ...and dies here. The lease outlives it in the store.
    }

    // The new process boots, recovers, and serves.
    let provider = Arc::new(MockProvider::new([reply("back, and whole.")]));
    let daemon = Daemon::build(
        &config(&dir, None),
        mock_registry(provider),
        rhema_rsd::DispatcherConfig::default(),
    )
    .expect("the daemon assembles");
    daemon.bootstrap().await.expect("recovery runs");

    let base = serve(&daemon).await;
    let client = reqwest::Client::new();

    // The key reads idle, not leased: the ghost lease is gone.
    let sessions: Value = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("sessions reads")
        .json()
        .await
        .expect("sessions parses");
    assert_eq!(sessions[0]["key"], "interactive:web");
    assert_eq!(sessions[0]["state"], "idle", "{sessions}");

    // The next message dispatches instead of queueing behind the ghost.
    let events = client
        .get(format!("{base}/v1/sessions/interactive:web/events"))
        .send()
        .await
        .expect("the events request goes out");
    let mut stream = sse_body(events).await;
    let mut seen = String::new();
    let accepted = client
        .post(format!("{base}/v1/sessions/interactive:web/messages"))
        .json(&json!({"text": "are you back?"}))
        .send()
        .await
        .expect("the message posts");
    assert_eq!(accepted.status(), 202);
    read_until(&mut stream, &mut seen, "back, and whole.").await;
    read_until(&mut stream, &mut seen, r#""type":"turn_done""#).await;
}
