//! End-to-end tests over the protocol: an in-memory client speaks ACP
//! to the served agent through a [`Channel`] pair, and a `MockProvider`
//! scripts the model side. The whole stack runs, handlers, session
//! slots, the event bridge, the permission bridge, persistence, with
//! zero network and zero editor.

#![cfg(feature = "mock")]

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    AuthenticateRequest, CancelNotification, ContentBlock, InitializeRequest, InitializeResponse,
    LoadSessionRequest, NewSessionRequest, NewSessionResponse, PermissionOptionId, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOption, SessionConfigOptionCategory, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, StopReason,
};
use agent_client_protocol::{Channel, Client, on_receive_notification, on_receive_request};
use rhema_acp::AcpOptions;
use rhema_core::{Content, Message, ModelOutput, Role, StopReason as EngineStop, ToolCall, Usage};
use rhema_session_store::{SessionId as StoreSessionId, SessionStore};
use rhema_session_store_file::FileStore;
use rhema_submissions::{Status, snapshot, turn};
use serde_json::json;

/// A text reply: `text` streamed and appended, turn over.
fn reply(text: &str) -> ModelOutput {
    reply_using(text, Usage::default())
}

/// A text reply that bills `usage`.
fn reply_using(text: &str, usage: Usage) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: EngineStop::EndTurn,
        usage,
    }
}

/// The token accounting one step bills.
fn usage(input: u32, output: u32, cached: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
    }
}

/// A reply that requests one `bash` call.
fn reply_call(id: &str, command: &str) -> ModelOutput {
    ModelOutput {
        message: Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "bash".to_owned(),
                args: serde_json::json!({ "command": command }),
                provider_data: serde_json::Value::Null,
            })],
        },
        stop_reason: EngineStop::ToolUse,
        usage: Usage::default(),
    }
}

/// The collected session updates, in arrival order.
type Updates = Arc<Mutex<Vec<SessionUpdate>>>;

fn updates() -> Updates {
    Arc::new(Mutex::new(Vec::new()))
}

fn collect_into(updates: &Updates, update: SessionUpdate) {
    updates.lock().unwrap().push(update);
}

/// The texts of the streamed agent-message chunks so far.
fn chunk_texts(updates: &Updates) -> Vec<String> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The statuses of the tool-call updates, in arrival order.
fn tool_updates(updates: &Updates) -> Vec<(String, String)> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ToolCallUpdate(call) => Some((
                call.tool_call_id.0.to_string(),
                format!("{:?}", call.fields.status),
            )),
            _ => None,
        })
        .collect()
}

/// The context-and-cost updates, as `(used, size, cost amount)`, the
/// cost `None` when the model card carries no prices.
fn usage_updates(updates: &Updates) -> Vec<(u64, u64, Option<f64>)> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::UsageUpdate(usage) => Some((
                usage.used,
                usage.size,
                usage.cost.as_ref().map(|cost| cost.amount),
            )),
            _ => None,
        })
        .collect()
}

/// The session-info updates, as `(title, updated_at)`.
fn info_updates(updates: &Updates) -> Vec<(Option<String>, Option<String>)> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::SessionInfoUpdate(info) => {
                let title = info.title.value().cloned();
                let updated_at = info.updated_at.value().cloned();
                Some((title, updated_at))
            }
            _ => None,
        })
        .collect()
}

/// The config-option updates a session pushed.
fn config_updates(updates: &Updates) -> Vec<Vec<SessionConfigOption>> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ConfigOptionUpdate(config) => Some(config.config_options.clone()),
            _ => None,
        })
        .collect()
}

/// The advertised command names, one list per update.
fn command_updates(updates: &Updates) -> Vec<Vec<String>> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::AvailableCommandsUpdate(commands) => Some(
                commands
                    .available_commands
                    .iter()
                    .map(|command| command.name.clone())
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

/// The thinking select's current value among `options`.
fn thinking_current(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find(|option| option.category.as_ref() == Some(&SessionConfigOptionCategory::ThoughtLevel))
        .and_then(|option| match &option.kind {
            agent_client_protocol::v1::schema::SessionConfigKind::Select(select) => {
                Some(select.current_value.0.to_string())
            }
            _ => None,
        })
}

fn options(replies: Vec<ModelOutput>, session_dir: &Path) -> AcpOptions {
    AcpOptions {
        session_dir: Some(session_dir.to_path_buf()),
        mock_replies: Some(replies),
        ..AcpOptions::default()
    }
}

/// The permission requests the agent sent, in arrival order.
type Permissions = Arc<Mutex<Vec<RequestPermissionRequest>>>;

/// Serves the agent on one end of a fresh channel pair; the test drives
/// the client end inside `scenario`. `answer` is every permission
/// request's outcome; the requests themselves land in `permissions`.
async fn with_agent<T>(
    options: AcpOptions,
    updates: Updates,
    permissions: Permissions,
    answer: RequestPermissionOutcome,
    scenario: impl AsyncFnOnce(
        agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Agent>,
    ) -> agent_client_protocol::Result<T>,
) -> agent_client_protocol::Result<T> {
    let (agent_end, client_end) = Channel::duplex();
    tokio::spawn(async move {
        let _ = rhema_acp::serve_over(agent_end, options).await;
    });
    Client
        .v1()
        .on_receive_notification(
            {
                let updates = updates.clone();
                async move |notification: SessionNotification, _cx| {
                    collect_into(&updates, notification.update);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                permissions.lock().unwrap().push(request);
                responder.respond(RequestPermissionResponse::new(answer.clone()))
            },
            on_receive_request!(),
        )
        .connect_with(client_end, async move |cx| scenario(cx).await)
        .await
}

async fn initialize(
    cx: &agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Agent>,
) -> InitializeResponse {
    cx.send_request(InitializeRequest::new(
        agent_client_protocol::schema::ProtocolVersion::V1,
    ))
    .block_task()
    .await
    .expect("initialize")
}

async fn new_session(
    cx: &agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Agent>,
    workspace: &Path,
) -> NewSessionResponse {
    cx.send_request(NewSessionRequest::new(workspace.to_path_buf()))
        .block_task()
        .await
        .expect("session/new")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_streams_and_the_session_serves_again() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply("hello agent"), reply("again")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            let init = initialize(&cx).await;
            assert!(init.agent_capabilities.load_session);
            assert_eq!(init.auth_methods.len(), 1);

            let session = new_session(&cx, &workspace).await.session_id;

            let first = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("hi")],
                ))
                .block_task()
                .await
                .expect("first prompt");
            assert_eq!(first.stop_reason, StopReason::EndTurn);
            assert_eq!(chunk_texts(&collected), vec!["hello agent".to_owned()]);

            // The agent returned to its slot: a second prompt serves.
            let second = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("more")],
                ))
                .block_task()
                .await
                .expect("second prompt");
            assert_eq!(second.stop_reason, StopReason::EndTurn);
            assert_eq!(
                chunk_texts(&collected),
                vec!["hello agent".to_owned(), "again".to_owned()]
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_round_trips_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply_call("call-1", "echo wired"), reply("done")],
            &session_dir,
        ),
        updates,
        permissions.clone(),
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("run it")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            // The permission request named the command, not the tool.
            let asked = permissions.lock().unwrap();
            assert_eq!(asked.len(), 1);
            assert_eq!(
                asked[0].tool_call.fields.title.as_deref(),
                Some("echo wired")
            );

            // The call opened pending, streamed its output as it ran,
            // and closed completed; the tool ran.
            let closed = tool_updates(&collected);
            assert!(closed.len() >= 2, "{:?}", collected.lock().unwrap());
            assert!(
                closed
                    .iter()
                    .any(|(id, status)| id == "call-1" && status.contains("InProgress"))
            );
            let last = closed.last().expect("at least two updates");
            assert_eq!(last.0, "call-1");
            assert!(last.1.contains("Completed"), "{closed:?}");
            assert_eq!(
                chunk_texts(&collected),
                vec!["done".to_owned()],
                "the assistant continued after the tool ran"
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_at_the_step_cap_ends_with_max_turn_requests() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let mut options = options(vec![reply_call("call-1", "echo capped")], &session_dir);
    options.max_steps = Some(NonZeroU32::new(1).unwrap());
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options,
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("run it")],
                ))
                .block_task()
                .await
                .expect("prompt");
            // The cap is not a protocol error: the call ran, and the
            // client sees the spec's max-turn-request stop.
            assert_eq!(response.stop_reason, StopReason::MaxTurnRequests);
            assert!(
                collected
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|update| matches!(update, SessionUpdate::ToolCall(_))),
                "the capped turn still reports its tool call"
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_call_fails_in_band() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply_call("call-1", "echo never"), reply("understood")],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("reject-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("run it")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            let closed = tool_updates(&collected);
            assert_eq!(closed.len(), 1);
            assert!(closed[0].1.contains("Failed"), "{closed:?}");
            assert_eq!(
                chunk_texts(&collected),
                vec!["understood".to_owned()],
                "the model saw the rejection and answered"
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_drops_a_hung_turn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    // The bash tool really runs `sleep 2`: a turn parked long enough
    // for the cancel to land deterministically.
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply_call("call-1", "sleep 2"), reply("unreachable")],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            // Wire order is dispatch order: the prompt handler installs
            // its cancel watcher before the cancel notification runs.
            let pending = cx.send_request(PromptRequest::new(
                session.clone(),
                vec![ContentBlock::from("hang")],
            ));
            cx.send_notification(CancelNotification::new(session.clone()))?;
            let response: PromptResponse = pending.block_task().await.expect("cancelled prompt");
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            // The dropped turn is a terminal record on disk, not a
            // missing one.
            let saved = saved_session(&session_dir, &session);
            let ledger = snapshot(&saved).expect("the ledger parses");
            let record = ledger.newest().expect("the prompt left a record");
            assert_eq!(record.kind.as_str(), "turn");
            assert_eq!(record.status, Status::Cancelled);
            assert_eq!(record.outcome, Some(rhema_submissions::Outcome::Cancelled));

            // The repaired session still serves.
            let after = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("still there?")],
                ))
                .block_task()
                .await
                .expect("post-cancel prompt");
            assert_eq!(after.stop_reason, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

/// The session a save landed under `id`, read back through the store the
/// agent writes.
fn saved_session(session_dir: &Path, id: &SessionId) -> rhema_core::Session {
    let id = StoreSessionId::new(id.0.to_string()).expect("the agent allocates valid ids");
    FileStore::new(session_dir)
        .load(&id)
        .expect("the checkpoint reads back")
        .expect("the checkpoint was committed")
}

/// Waits until the sink announced the tool call, which the engine emits
/// only after its commit-before-execute, so the checkpoint on disk
/// already holds the call.
async fn wait_for_call(updates: &Updates, id: &str) {
    for _ in 0..500 {
        let seen = updates.lock().unwrap().iter().any(|update| match update {
            SessionUpdate::ToolCall(call) => call.tool_call_id.0.as_ref() == id,
            _ => false,
        });
        if seen {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the tool call {id} was never announced");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_tool_call_leaves_the_committed_intent_and_a_reload_closes_it() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply_call("call-1", "sleep 2"), reply("recovered")],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;
            let pending = cx.send_request(PromptRequest::new(
                session.clone(),
                vec![ContentBlock::from("run the command")],
            ));
            // The call's start event arrives after the intent commit,
            // so this cancel lands with the call durably on disk and
            // its result not yet produced.
            wait_for_call(&collected, "call-1").await;
            cx.send_notification(CancelNotification::new(session.clone()))?;
            let response: PromptResponse = pending.block_task().await.expect("cancelled turn");
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            // The commit hook survives the turn's death: the file ends
            // with the assistant call and no answer to it.
            let saved = saved_session(&session_dir, &session);
            let last = saved.history.last().expect("the prompt was committed");
            assert_eq!(last.role, Role::Assistant);
            assert!(
                last.content
                    .iter()
                    .any(|content| matches!(content, Content::ToolCall(call) if call.id == "call-1")),
                "the committed intent carries the call: {last:?}"
            );
            assert!(
                !saved
                    .history
                    .iter()
                    .any(|message| message.role == Role::Tool),
                "the result never reached disk: {saved:?}"
            );

            // A reopened agent revives the dangling call before its next
            // request, and the turn serves.
            cx.send_request(LoadSessionRequest::new(
                session.clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("session/load");
            let after = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("continue")],
                ))
                .block_task()
                .await
                .expect("post-load prompt");
            assert_eq!(after.stop_reason, StopReason::EndTurn);

            // The revive's answer is committed with the next turn.
            let saved = saved_session(&session_dir, &session);
            assert!(
                saved.history.iter().any(|message| message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::ToolResult(result) if result.id == "call-1"))),
                "revive closed the call: {saved:?}"
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_keeps_the_committed_step_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    // Exactly one reply: the tool call runs, then the next step's
    // provider request finds the script exhausted and fails the turn.
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply_call("call-1", "echo committed")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;
            let failed = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("run it")],
                ))
                .block_task()
                .await;
            assert!(failed.is_err(), "the exhausted provider fails the turn");

            // Before the commit hook this failure wrote nothing; the
            // whole step now survives for the next resume.
            let saved = saved_session(&session_dir, &session);
            assert!(saved.history.iter().any(|message| message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolCall(call) if call.id == "call-1"))));
            assert!(saved.history.iter().any(|message| message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolResult(result) if result.id == "call-1"))));

            // The failure is classed at settle time: an exhausted script
            // is not a transport death, so a retry cannot fix it.
            let ledger = snapshot(&saved).expect("the ledger parses");
            let record = ledger.newest().expect("the prompt left a record");
            assert_eq!(record.status, Status::Failed);
            assert!(matches!(
                record.outcome,
                Some(rhema_submissions::Outcome::Failed {
                    retryable: false,
                    ..
                })
            ));
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_load_replays_history_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply("hello agent"), reply("hello agent")],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let session: SessionId = new_session(&cx, &workspace).await.session_id;

            cx.send_request(PromptRequest::new(
                session.clone(),
                vec![ContentBlock::from("hi")],
            ))
            .block_task()
            .await
            .expect("first prompt");

            // A fresh chunk of the collected updates starts here.
            let mark = collected.lock().unwrap().len();

            cx.send_request(LoadSessionRequest::new(
                session.clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("session/load");

            let replay: Vec<String> = collected
                .lock()
                .unwrap()
                .iter()
                .skip(mark)
                .filter_map(|update| match update {
                    SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
                        ContentBlock::Text(text) => Some(format!("user: {}", text.text)),
                        _ => None,
                    },
                    SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                        ContentBlock::Text(text) => Some(format!("agent: {}", text.text)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(
                replay,
                vec!["user: hi".to_owned(), "agent: hello agent".to_owned()]
            );

            // The restored session serves a turn of its own.
            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("more")],
                ))
                .block_task()
                .await
                .expect("post-load prompt");
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticate_reports_unresolvable_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    // No mock backend: resolution falls through to an injected empty
    // config, so no credential can resolve however the real one looks.
    let options = AcpOptions {
        session_dir: Some(session_dir),
        provider: Some("openrouter".to_owned()),
        config: Some(rhema_config::Settings::default()),
        ..AcpOptions::default()
    };
    if std::env::var_os("OPENROUTER_API_KEY").is_some() {
        return;
    }
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options,
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let error = cx
                .send_request(AuthenticateRequest::new(
                    agent_client_protocol::v1::schema::AuthMethodId::new("rhema-stored"),
                ))
                .block_task()
                .await
                .expect_err("authenticate must fail without credentials");
            let data = error.data.clone().unwrap_or(serde_json::Value::Null);
            assert!(
                data.to_string().contains("credentials"),
                "the error names the fix: {error:?}"
            );

            // The code editors check to offer their login flow.
            let session = cx
                .send_request(NewSessionRequest::new(workspace.to_path_buf()))
                .block_task()
                .await
                .expect_err("session/new must demand auth without credentials");
            assert_eq!(
                session.code,
                agent_client_protocol::v1::schema::ErrorCode::AuthRequired
            );
            Ok(())
        },
    )
    .await;
    outcome.expect("authenticate scenario");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_picker_advertises_the_active_model_and_switches_it() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    // Two scripted replies so a turn runs on the first model and another
    // on the switched one.
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply("first"), reply("second")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await;

            // session/new advertises a Model select whose current value
            // is the model the agent runs on, in the `<kind>/<model>`
            // shape every choice value carries ("mock" under the mock
            // feature), and the choice's description names its provider.
            let select = session
                .config_options
                .as_ref()
                .and_then(|options| {
                    options.iter().find(|option| {
                        option.category.as_ref() == Some(&SessionConfigOptionCategory::Model)
                    })
                })
                .expect("session/new advertises a Model select");
            let kind = match &select.kind {
                agent_client_protocol::v1::schema::SessionConfigKind::Select(select) => select,
                _ => panic!("the Model option is a select"),
            };
            assert_eq!(kind.current_value.0.as_ref(), "mock/mock");
            let agent_client_protocol::v1::schema::SessionConfigSelectOptions::Ungrouped(choices) =
                &kind.options
            else {
                panic!("the Model select's choices are ungrouped");
            };
            let first = choices
                .iter()
                .find(|option| option.value.0.as_ref() == "mock/mock")
                .expect("the current value is among the choices");
            // The description names the provider and, now the mock card
            // carries a rate and a window, the picker shows them too.
            assert!(
                first
                    .description
                    .as_deref()
                    .is_some_and(|description| description.starts_with("mock · $1.00/M in")),
                "{}",
                first.description.as_deref().unwrap_or_default()
            );

            // A first turn runs on the advertised model.
            let first = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("hi")],
                ))
                .block_task()
                .await
                .expect("first prompt");
            assert_eq!(first.stop_reason, StopReason::EndTurn);

            // A choice naming an unknown provider kind is a named
            // parameter error, not a model id leaking to the provider.
            let bogus = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "model",
                    "bogus/model",
                ))
                .block_task()
                .await
                .expect_err("an unknown kind must not switch");
            assert_eq!(
                bogus.code,
                agent_client_protocol::v1::schema::ErrorCode::InvalidParams
            );

            // A model switch rebuilds the agent over the history; the
            // response's select now names the new model as current. The
            // mock kind's own prefix strips without a provider rebuild.
            let switched = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "model",
                    "mock/other-model",
                ))
                .block_task()
                .await
                .expect("set_config_option");
            let select = switched
                .config_options
                .iter()
                .find(|option| {
                    option.category.as_ref() == Some(&SessionConfigOptionCategory::Model)
                })
                .expect("the response carries the Model select");
            match &select.kind {
                agent_client_protocol::v1::schema::SessionConfigKind::Select(select) => {
                    assert_eq!(select.current_value.0.as_ref(), "mock/other-model");
                }
                _ => panic!("the Model option is a select"),
            }

            // A bare value keeps the session's provider, the compatibility
            // shape for hand-written clients.
            let bare = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "model",
                    "third-model",
                ))
                .block_task()
                .await
                .expect("bare set_config_option");
            let select = bare
                .config_options
                .iter()
                .find(|option| {
                    option.category.as_ref() == Some(&SessionConfigOptionCategory::Model)
                })
                .expect("the response carries the Model select");
            match &select.kind {
                agent_client_protocol::v1::schema::SessionConfigKind::Select(select) => {
                    assert_eq!(select.current_value.0.as_ref(), "mock/third-model");
                }
                _ => panic!("the Model option is a select"),
            }

            // The session continues on the new model: a second turn runs.
            let second = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("more")],
                ))
                .block_task()
                .await
                .expect("second prompt");
            assert_eq!(second.stop_reason, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_reports_usage_context_cost_and_a_title() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![
                reply_using("hello agent", usage(1_000, 100, 400)),
                reply_using("again", usage(2_000, 50, 0)),
            ],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            let first = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("hi there, help")],
                ))
                .block_task()
                .await
                .expect("first prompt");
            assert_eq!(first.stop_reason, StopReason::EndTurn);
            // The response carries the session's running totals.
            let billed = first.usage.expect("the response carries usage");
            assert_eq!(billed.total_tokens, 1_100);
            assert_eq!(billed.input_tokens, 1_000);
            assert_eq!(billed.output_tokens, 100);
            assert_eq!(billed.cached_read_tokens, Some(400));

            // The step left its context and cost on the wire: 1_100
            // tokens in a 200k window, 600 uncached input at $1/M plus
            // 400 cached at the quarter fallback plus 100 output at
            // $3/M.
            let seen = usage_updates(&collected);
            let last = seen.last().expect("a usage update per turn");
            assert_eq!(*last, (1_100, 200_000, Some(0.001)), "{seen:?}");

            // The session titled itself from the first prompt and
            // stamped its activity.
            let infos = info_updates(&collected);
            let (title, updated_at) = infos.last().expect("an info update per turn");
            assert_eq!(title.as_deref(), Some("hi there, help"));
            assert!(
                updated_at
                    .as_deref()
                    .is_some_and(|at| at.ends_with('Z') && at.len() > 20),
                "{updated_at:?}"
            );

            let second = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("more")],
                ))
                .block_task()
                .await
                .expect("second prompt");
            assert_eq!(second.stop_reason, StopReason::EndTurn);
            let billed = second.usage.expect("the response carries usage");
            assert_eq!(billed.total_tokens, 3_150);
            assert_eq!(billed.input_tokens, 3_000);
            assert_eq!(billed.output_tokens, 150);

            // The context now rides the second step's shape, and the
            // cost accumulated: 2_000 input at $1/M plus 50 output at
            // $3/M on top of the first turn's 0.001.
            let seen = usage_updates(&collected);
            let last = seen.last().expect("a usage update per turn");
            assert_eq!(last.0, 2_050);
            assert_eq!(last.1, 200_000);
            let cost = last.2.expect("the mock card prices its model");
            assert!((cost - 0.00315).abs() < 1e-9, "{cost}");

            // The title rode once; this update stamps activity only.
            let infos = info_updates(&collected);
            let (title, updated_at) = infos.last().expect("an info update per turn");
            assert_eq!(title.as_deref(), None);
            assert!(updated_at.is_some());
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_thinking_select_switches_effort() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply("first"), reply("second")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await;

            // Both selects ship: the model and, current first, the
            // profile's reasoning effort, medium when nothing stored
            // names one.
            assert_eq!(session.config_options.as_ref().map(Vec::len), Some(2));
            let current = thinking_current(session.config_options.as_ref().expect("selects"));
            assert_eq!(current.as_deref(), Some("medium"));

            // The select exposes exactly the three levels, capitalized,
            // each naming its tradeoff; `default` is not among them.
            let thinking = session
                .config_options
                .as_ref()
                .expect("selects")
                .iter()
                .find(|option| {
                    option.category.as_ref() == Some(&SessionConfigOptionCategory::ThoughtLevel)
                })
                .expect("the thinking select ships");
            let agent_client_protocol::v1::schema::SessionConfigKind::Select(select) =
                &thinking.kind
            else {
                panic!("the thinking option is a select");
            };
            let agent_client_protocol::v1::schema::SessionConfigSelectOptions::Ungrouped(choices) =
                &select.options
            else {
                panic!("the thinking select's choices are ungrouped");
            };
            let ids: Vec<&str> = choices
                .iter()
                .map(|option| option.value.0.as_ref())
                .collect();
            assert_eq!(ids, ["low", "medium", "high"]);
            let names: Vec<&str> = choices.iter().map(|option| option.name.as_str()).collect();
            assert_eq!(names, ["Low", "Medium", "High"]);
            assert!(
                choices.iter().all(|option| option
                    .description
                    .as_deref()
                    .is_some_and(|text| !text.is_empty())),
                "every level names its tradeoff"
            );

            // `default` is not exposed: switching to it is a named
            // parameter error, as for any unknown value.
            let default = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "thinking",
                    "default",
                ))
                .block_task()
                .await
                .expect_err("default is not an exposed level");
            assert_eq!(
                default.code,
                agent_client_protocol::v1::schema::ErrorCode::InvalidParams
            );

            // An unknown effort is a named parameter error.
            let bogus = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "thinking",
                    "turbo",
                ))
                .block_task()
                .await
                .expect_err("an unknown effort must not switch");
            assert_eq!(
                bogus.code,
                agent_client_protocol::v1::schema::ErrorCode::InvalidParams
            );

            // Switching rebuilds the agent over the same history; the
            // response carries both selects with the new current.
            let switched = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "thinking",
                    "high",
                ))
                .block_task()
                .await
                .expect("set_config_option");
            assert_eq!(
                thinking_current(&switched.config_options).as_deref(),
                Some("high")
            );

            // The session still serves turns on the rebuilt agent.
            let first = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("hi")],
                ))
                .block_task()
                .await
                .expect("first prompt");
            assert_eq!(first.stop_reason, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_commands_switch_and_confirm_in_band() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply("hello agent")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;

            // session/new advertised the commands the session takes.
            assert_eq!(
                command_updates(&collected).last(),
                Some(&vec!["thinking".to_owned(), "model".to_owned()]),
                "{:?}",
                collected.lock().unwrap()
            );

            // /thinking switches in place: a confirmation streams as
            // the turn's message and the pickers re-render.
            let answer = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("/thinking high")],
                ))
                .block_task()
                .await
                .expect("/thinking");
            assert_eq!(answer.stop_reason, StopReason::EndTurn);
            assert_eq!(
                chunk_texts(&collected).last(),
                Some(&"reasoning effort set to high".to_owned())
            );
            let pushed = config_updates(&collected);
            assert_eq!(
                thinking_current(pushed.last().expect("selects ride along")).as_deref(),
                Some("high")
            );

            // A bare /model lists the current model and its choices.
            let answer = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("/model")],
                ))
                .block_task()
                .await
                .expect("/model");
            assert_eq!(answer.stop_reason, StopReason::EndTurn);
            assert!(
                chunk_texts(&collected)
                    .last()
                    .is_some_and(|message| message.contains("model is mock/mock")),
                "{:?}",
                chunk_texts(&collected)
            );

            // A bad argument answers in band with the valid ones.
            let answer = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("/thinking turbo")],
                ))
                .block_task()
                .await
                .expect("/thinking turbo");
            assert_eq!(answer.stop_reason, StopReason::EndTurn);
            assert!(
                chunk_texts(&collected)
                    .last()
                    .is_some_and(|message| message.contains("low, medium, or high"))
            );

            // Any other slash spelling is model input like any other
            // text: the turn runs and the model answers.
            let answer = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("/no/such command for real")],
                ))
                .block_task()
                .await
                .expect("an unknown command falls through");
            assert_eq!(answer.stop_reason, StopReason::EndTurn);
            assert_eq!(
                chunk_texts(&collected).last(),
                Some(&"hello agent".to_owned())
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_slot_map_stays_capped_and_a_dropped_session_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();
    let collected = updates.clone();
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    // More sessions than the cap, one prompt in the middle so its
    // session persists and can come back from disk after eviction.
    let replies: Vec<ModelOutput> = (0..3).map(|i| reply(&format!("r{i}"))).collect();
    let outcome = with_agent(
        options(replies, &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Cancelled,
        async move |cx| {
            initialize(&cx).await;
            let mut ids = Vec::new();
            for i in 0..20 {
                let res = new_session(&cx, &workspace).await;
                ids.push(res.session_id.clone());
                if i == 1 {
                    cx.send_request(PromptRequest::new(
                        res.session_id.clone(),
                        vec![ContentBlock::from("hi")],
                    ))
                    .block_task()
                    .await
                    .expect("prompt");
                }
            }
            assert!(
                ids.windows(2).all(|w| w[0] != w[1]),
                "every session gets its own id"
            );
            // The evicted, prompted session still comes back whole.
            cx.send_request(LoadSessionRequest::new(
                ids[1].clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("session/load after eviction");
            // The load answered, and the reloaded session serves a
            // prompt, the eviction lost nothing that matters.
            cx.send_request(PromptRequest::new(
                ids[1].clone(),
                vec![ContentBlock::from("again")],
            ))
            .block_task()
            .await
            .expect("prompt on the reloaded session");
            Ok(())
        },
    )
    .await;
    outcome.expect("scenario");
    let _ = collected;
}

/// Rewrites the saved session as a process that died after the last
/// committed boundary: the turn record back to `Running` with its
/// settle cleared, and the history cut at `len`.
fn kill_in_place(session_dir: &Path, session: &SessionId, len: usize) -> std::time::SystemTime {
    let id = StoreSessionId::new(session.0.to_string()).expect("the agent allocates valid ids");
    let store = FileStore::new(session_dir);
    let mut saved = store.load(&id).expect("the checkpoint reads back").unwrap();
    saved.history.truncate(len);
    let ledger = snapshot(&saved).expect("the ledger parses");
    let record = ledger.newest().expect("the prompt left a record");
    let item = &mut saved.driver_state["submissions"]["items"][record.id.as_str()];
    item["status"] = json!("running");
    item["settled_at"] = json!(null);
    item["outcome"] = json!(null);
    store
        .save(&id, &saved)
        .expect("the killed session is written");
    std::fs::metadata(store.path_for(&id))
        .expect("the file exists")
        .modified()
        .expect("the file has a timestamp")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_leaves_a_done_record_with_the_answer_in_history() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(vec![reply("the answer")], &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;
            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("fix the test")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            let saved = saved_session(&session_dir, &session);
            let ledger = snapshot(&saved).expect("the ledger parses");
            assert_eq!(ledger.len(), 1, "one prompt, one record");
            let record = ledger.newest().unwrap();
            assert_eq!(record.kind.as_str(), "turn");
            assert_eq!(record.status, Status::Done);
            assert_eq!(record.attempts, 1);
            assert!(record.started_at.is_some() && record.settled_at.is_some());
            assert_eq!(
                turn::answer(&saved, &ledger, &record.id).as_deref(),
                Some("the answer")
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_killed_after_a_boundary_reconciles_to_interrupted_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");
    let updates = updates();

    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(
            vec![reply_call("call-1", "echo committed"), reply("done")],
            &session_dir,
        ),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session = new_session(&cx, &workspace).await.session_id;
            let response = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("run it")],
                ))
                .block_task()
                .await
                .expect("prompt");
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            // A process that died before settling: the tool result is
            // committed, the answer is not.
            let before = kill_in_place(&session_dir, &session, 3);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            cx.send_request(LoadSessionRequest::new(
                session.clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("session/load");

            let saved = saved_session(&session_dir, &session);
            let ledger = snapshot(&saved).expect("the ledger parses");
            let record = ledger.newest().unwrap();
            assert_eq!(record.status, Status::Interrupted);
            assert_eq!(
                turn::answer(&saved, &ledger, &record.id),
                None,
                "no answer was committed"
            );
            assert_eq!(saved.history.len(), 3, "the load appended nothing");

            // The first load reconciled and wrote; the second has
            // nothing to change and leaves the file alone.
            let id = StoreSessionId::new(session.0.to_string()).unwrap();
            let path = FileStore::new(&session_dir).path_for(&id);
            let reconciled = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_ne!(reconciled, before, "the reconcile landed once");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            cx.send_request(LoadSessionRequest::new(
                session.clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("second session/load");
            let unchanged = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(
                unchanged, reconciled,
                "a reconnect does not rewrite the session"
            );

            // The interrupted record still serves a new prompt.
            let after = cx
                .send_request(PromptRequest::new(
                    session.clone(),
                    vec![ContentBlock::from("continue")],
                ))
                .block_task()
                .await
                .expect("post-load prompt");
            assert_eq!(after.stop_reason, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_load_reconciles_an_unregistered_kind_to_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = dir.path().join("sessions");

    // A session written by a build that links a `job` kind: this one
    // does not, so the record must survive as an unclaimed one.
    let id = StoreSessionId::new("0001759000000000-00").unwrap();
    let mut session = rhema_core::Session {
        history: vec![Message::text(Role::User, "run something")],
        ..rhema_core::Session::default()
    };
    let record = rhema_submissions::submit(
        &mut session,
        rhema_submissions::KindName::new("job").unwrap(),
        json!({ "command": "sleep 1" }),
        1,
    )
    .unwrap();
    rhema_submissions::start(&mut session, &record.id, 2, None).unwrap();
    FileStore::new(&session_dir)
        .save(&id, &session)
        .expect("the crafted session is saved");

    let updates = updates();
    let permissions: Permissions = Arc::new(Mutex::new(Vec::new()));
    let outcome = with_agent(
        options(Vec::new(), &session_dir),
        updates,
        permissions,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new("allow-once"),
        )),
        async move |cx| {
            initialize(&cx).await;
            let session_id = SessionId::new(id.as_str().to_owned());
            cx.send_request(LoadSessionRequest::new(
                session_id.clone(),
                workspace.to_path_buf(),
            ))
            .block_task()
            .await
            .expect("an unregistered kind does not block the load");

            let saved = saved_session(&session_dir, &session_id);
            let ledger = snapshot(&saved).expect("the ledger parses");
            let record = ledger.newest().unwrap();
            assert_eq!(record.kind.as_str(), "job");
            assert_eq!(record.status, Status::Interrupted);
            assert_eq!(
                record.payload,
                json!({ "command": "sleep 1" }),
                "the payload is never touched"
            );
            Ok(())
        },
    )
    .await;
    let _ = outcome;
}
