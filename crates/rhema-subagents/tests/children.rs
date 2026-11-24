//! Child sessions and the parent's `subagent` records: adoption from
//! the store, recovery from the child's own ledger, and a resume that
//! continues the tail.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rhema_bash::{AllowAll, ShellTools};
use rhema_core::{
    Content, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
    Session, StopReason, ToolCall, ToolContext, ToolResolver, Usage,
};
use rhema_session_store::{SessionId, SessionStore};
use rhema_session_store_file::FileStore;
use rhema_subagents::kind::{self, Descriptor, Payload, SubagentKind};
use rhema_submissions::{
    Change, Kind, Kinds, Outcome, RecoverCtx, Recovery, Status, reconcile, snapshot, start, submit,
    turn,
};

/// Replies with one final answer to every request.
struct Answer;

#[async_trait]
impl ModelProvider for Answer {
    async fn complete(
        &self,
        _req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, "the rest of the work"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}

fn parent_id() -> SessionId {
    SessionId::new("0000000000000001-00").unwrap()
}

fn provider() -> Arc<dyn ModelProvider> {
    Arc::new(Answer)
}

fn def() -> rhema_subagents::SubagentDef {
    rhema_subagents::SubagentDef {
        name: "worker".into(),
        description: "works".into(),
        model: None,
        max_steps: None,
        body: "Work.".into(),
    }
}

/// A child session with a descriptor and one turn record in `status`.
fn child_session(_child: &SessionId, parent: &SessionId, status: Status) -> Session {
    let mut session = Session::default();
    kind::write_descriptor(
        &mut session,
        &Descriptor {
            def: "worker".into(),
            prompt: "do the work".into(),
            model: None,
            parent: parent.clone(),
        },
    );
    let queued = turn::submit(&mut session, "do the work", 1).unwrap();
    let started = turn::start(&mut session, &queued.id, "do the work", 2).unwrap();
    session
        .history
        .push(Message::text(Role::User, "do the work"));
    match status {
        Status::Running => session.history.push(Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "true" }),
                ..ToolCall::default()
            })],
        }),
        Status::Done => {
            session
                .history
                .push(Message::text(Role::Assistant, "the report"));
            rhema_submissions::settle(&mut session, &started.id, Outcome::Done, 3).unwrap();
        }
        Status::Interrupted => {
            session.history.push(Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({ "command": "true" }),
                    ..ToolCall::default()
                })],
            });
            rhema_submissions::reconcile(&mut session, &Kinds::new(), 3).unwrap();
        }
        _ => {}
    }
    session
}

#[test]
fn a_done_child_is_adopted_and_settles_the_parent_done() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    store
        .save(&child, &child_session(&child, &parent, Status::Done))
        .unwrap();
    let _ = store;

    let kinds = Kinds::new().with(Arc::new(SubagentKind::new(
        Arc::new(FileStore::new(dir.path())),
        parent.clone(),
    )));
    let mut parent_session = Session::default();
    let changes = reconcile(&mut parent_session, &kinds, 10).unwrap();

    assert_eq!(changes.len(), 1);
    let Change::Recovered {
        kind: tag,
        to: Status::Done,
        id,
    } = &changes[0]
    else {
        panic!("unexpected change: {:?}", changes[0]);
    };
    assert_eq!(tag.as_str(), "subagent");
    let after = snapshot(&parent_session).unwrap();
    let record = after.get(id).unwrap();
    let payload: Payload = serde_json::from_value(record.payload.clone()).unwrap();
    assert_eq!(payload.child, child);
    assert_eq!(payload.prompt, "do the work");
    assert_eq!(record.status, Status::Done);

    // Nothing is adopted twice.
    assert!(
        reconcile(&mut parent_session, &kinds, 11)
            .unwrap()
            .is_empty()
    );
    assert_eq!(snapshot(&parent_session).unwrap().len(), 1);
}

#[test]
fn a_running_child_with_a_working_tail_is_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    store
        .save(&child, &child_session(&child, &parent, Status::Running))
        .unwrap();

    let kind = SubagentKind::new(Arc::new(FileStore::new(dir.path())), parent.clone());
    let mut parent_session = Session::default();
    submit(
        &mut parent_session,
        kind::kind(),
        serde_json::to_value(Payload {
            child: child.clone(),
            def: "worker".into(),
            prompt: "do the work".into(),
            model: None,
        })
        .unwrap(),
        1,
    )
    .unwrap();
    let id = snapshot(&parent_session)
        .unwrap()
        .newest()
        .unwrap()
        .id
        .clone();
    start(&mut parent_session, &id, 2, None).unwrap();

    let record = snapshot(&parent_session)
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap();
    let child_session = store.load(&child).unwrap().unwrap();
    assert_eq!(
        kind.recover(&RecoverCtx {
            session: &child_session,
            record: &record,
        }),
        Recovery::Interrupted
    );

    let changes = reconcile(
        &mut parent_session,
        &Kinds::new().with(Arc::new(SubagentKind::new(
            Arc::new(FileStore::new(dir.path())),
            parent,
        ))),
        10,
    )
    .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        snapshot(&parent_session).unwrap().get(&id).unwrap().status,
        Status::Interrupted
    );
}

#[test]
fn a_missing_child_settles_failed_and_never_retries() {
    let dir = tempfile::tempdir().unwrap();
    let parent = parent_id();
    let kind = SubagentKind::new(Arc::new(FileStore::new(dir.path())), parent);
    let mut session = Session::default();
    submit(
        &mut session,
        kind::kind(),
        serde_json::to_value(Payload {
            child: SessionId::new("0000000000000001-00.sub-0000000000000009").unwrap(),
            def: "worker".into(),
            prompt: "do the work".into(),
            model: None,
        })
        .unwrap(),
        1,
    )
    .unwrap();
    let id = snapshot(&session).unwrap().newest().unwrap().id.clone();
    start(&mut session, &id, 2, None).unwrap();

    let changes = reconcile(&mut session, &Kinds::new().with(Arc::new(kind)), 3).unwrap();
    assert_eq!(changes.len(), 1);
    let record = snapshot(&session).unwrap().get(&id).cloned().unwrap();
    assert_eq!(record.status, Status::Failed);
    assert!(matches!(
        record.outcome,
        Some(Outcome::Failed {
            retryable: false,
            ..
        })
    ));
    assert!(
        rhema_submissions::retry(&mut session, &id, 4).is_err(),
        "a missing child cannot be retried"
    );
}

#[tokio::test]
async fn a_resumed_child_continues_its_tail_and_settles_both_records() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    store
        .save(&child, &child_session(&child, &parent, Status::Running))
        .unwrap();

    let tools = rhema_subagents::SubagentTools::new(
        provider(),
        vec![def()],
        Arc::new(ShellTools::new(dir.path())),
        "test-model",
    );
    tools.attach_store(store.clone(), parent.clone());
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        run_id: "turn-1".into(),
        ..ToolContext::default()
    };

    let report = tools.resume(&child, &ctx).await.expect("the tail resumes");
    assert_eq!(report, "the rest of the work");

    // The child's ledger and tail both say done, and the parent's pass
    // reads the child rather than any copy.
    let resumed = store.load(&child).unwrap().unwrap();
    let newest = snapshot(&resumed)
        .unwrap()
        .newest()
        .cloned()
        .expect("the child keeps its turn record");
    assert_eq!(newest.status, Status::Done);
    assert!(
        resumed
            .history
            .last()
            .is_some_and(|message| message.role == Role::Assistant)
    );

    let mut parent_session = Session::default();
    let changes = reconcile(
        &mut parent_session,
        &Kinds::new().with(Arc::new(SubagentKind::new(store, parent))),
        10,
    )
    .unwrap();
    assert_eq!(changes.len(), 1);
    let record = snapshot(&parent_session).unwrap().newest().unwrap().clone();
    assert_eq!(record.status, Status::Done);
    assert_eq!(
        kind::report(&resumed).as_deref(),
        Some("the rest of the work")
    );
}

#[tokio::test]
async fn a_background_run_writes_a_child_session_and_announces_it() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let tools = Arc::new(rhema_subagents::SubagentTools::new(
        provider(),
        vec![def()],
        Arc::new(ShellTools::new(dir.path())),
        "test-model",
    ));
    tools.attach_store(store.clone(), parent.clone());
    let handle = tools.handle();

    let result = ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({ "name": "worker", "prompt": "do the work" }),
            provider_data: serde_json::Value::Null,
        },
        &ToolContext {
            cwd: dir.path().to_path_buf(),
            run_id: "turn-1".into(),
            ..ToolContext::default()
        },
    )
    .await;
    assert!(!result.is_error, "{result:?}");

    // The start named its child before the first model call, and the
    // frontend can drain it into the parent's ledger.
    let started = handle.take_starts();
    assert_eq!(started.len(), 1);
    let child = started[0].child.clone();
    assert!(kind::is_child(&child));
    assert!(kind::is_child_of(&parent, &child));
    assert!(handle.take_starts().is_empty(), "a drain empties the list");

    // Wait for the run to settle into the store.
    for _ in 0..200 {
        if store
            .load(&child)
            .unwrap()
            .is_some_and(|session| kind::finished(&session))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let session = store.load(&child).unwrap().expect("the child was saved");
    let descriptor = kind::descriptor(&session).expect("the descriptor was written");
    assert_eq!(descriptor.parent, parent);
    assert_eq!(descriptor.prompt, "do the work");
    assert_eq!(
        kind::report(&session).as_deref(),
        Some("the rest of the work")
    );
    assert_eq!(
        snapshot(&session).unwrap().newest().unwrap().status,
        Status::Done
    );

    // The parent rebuilds the same record from the child.
    let mut parent_session = Session::default();
    reconcile(
        &mut parent_session,
        &Kinds::new().with(Arc::new(SubagentKind::new(store, parent))),
        10,
    )
    .unwrap();
    let record = snapshot(&parent_session).unwrap().newest().unwrap().clone();
    assert_eq!(record.status, Status::Done);
    let payload: Payload = serde_json::from_value(record.payload).unwrap();
    assert_eq!(payload.child, child);
}

#[tokio::test]
async fn a_finished_child_fetches_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    store
        .save(&child, &child_session(&child, &parent, Status::Done))
        .unwrap();

    // A fresh process: no registry memory, only the store.
    let tools = rhema_subagents::SubagentTools::new(
        provider(),
        vec![def()],
        Arc::new(ShellTools::new(dir.path())),
        "test-model",
    );
    tools.attach_store(store, parent);
    let result = tools
        .resolve(
            ToolCall {
                id: "c1".into(),
                name: "subagent".into(),
                args: serde_json::json!({ "id": child.as_str() }),
                ..ToolCall::default()
            },
            &ToolContext {
                cwd: dir.path().to_path_buf(),
                ..ToolContext::default()
            },
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "the report");
}

#[test]
fn a_retryable_failure_never_reads_interrupted_on_recovery() {
    // A child that settled `Failed { retryable: true }` before its
    // process died (a provider 429, a dropped connection) must
    // recover as Failed: the retryable flag is the in-session ladder's
    // business, not the next load's. The old mapping through
    // `Interrupted` made every resume restart the child behind the
    // user's back.
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    // A `Running` child (descriptor, started turn, a dangling tail)
    // settled by the child's own driver into a retryable failure: the
    // exact shape a provider 429 leaves behind.
    let mut child_session = child_session(&child, &parent, Status::Running);
    let turn_id = snapshot(&child_session)
        .unwrap()
        .iter()
        .find(|record| record.kind == turn::kind())
        .unwrap()
        .id
        .clone();
    rhema_submissions::settle(
        &mut child_session,
        &turn_id,
        Outcome::Failed {
            error: "provider api error (HTTP 429): Rate limit exceeded.".into(),
            retryable: true,
        },
        3,
    )
    .unwrap();
    store.save(&child, &child_session).unwrap();

    let mut parent_session = Session::default();
    submit(
        &mut parent_session,
        kind::kind(),
        serde_json::to_value(Payload {
            child: child.clone(),
            def: "worker".into(),
            prompt: "do the work".into(),
            model: None,
        })
        .unwrap(),
        1,
    )
    .unwrap();
    let id = snapshot(&parent_session)
        .unwrap()
        .newest()
        .unwrap()
        .id
        .clone();
    start(&mut parent_session, &id, 2, None).unwrap();

    let changes = reconcile(
        &mut parent_session,
        &Kinds::new().with(Arc::new(SubagentKind::new(
            Arc::new(FileStore::new(dir.path())),
            parent.clone(),
        ))),
        10,
    )
    .unwrap();
    assert_eq!(changes.len(), 1);
    let record = snapshot(&parent_session)
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap();
    assert_eq!(record.status, Status::Failed);
    // A recovered verdict is a tombstone: the flag flattens to
    // `false`, so the explicit retry verb refuses this record too and
    // the only path back is a fresh request with the same recipe.
    assert!(matches!(
        record.outcome,
        Some(Outcome::Failed {
            retryable: false,
            ..
        })
    ));
    assert!(
        rhema_submissions::retry(&mut parent_session, &id, 15).is_err(),
        "a recovered failure cannot be retried"
    );
    // And a second load sees a settled record, not stale work: no
    // further recovery fires.
    let changes = reconcile(
        &mut parent_session,
        &Kinds::new().with(Arc::new(SubagentKind::new(
            Arc::new(FileStore::new(dir.path())),
            parent,
        ))),
        20,
    )
    .unwrap();
    assert!(changes.is_empty());
}

/// One tool-use step, then a dead link once, then the report on every
/// later call: the exact shape a transient failure leaves a child in,
/// the first run with work committed before it dies, the restart
/// picking the tail up over a recovered network.
struct Outage {
    served: AtomicUsize,
}

#[async_trait]
impl ModelProvider for Outage {
    async fn complete(
        &self,
        _req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        match self.served.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(ModelOutput {
                message: Message {
                    annotations: Default::default(),
                    role: Role::Assistant,
                    content: vec![Content::ToolCall(ToolCall {
                        id: "inner-1".into(),
                        name: "bash".into(),
                        args: serde_json::json!({ "command": "echo step-one" }),
                        ..ToolCall::default()
                    })],
                },
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }),
            1 => Err(ProviderError::Network("dead link".into())),
            _ => Ok(ModelOutput {
                message: Message::text(Role::Assistant, "the report after the outage"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        }
    }
}

/// The job id a start result names: what a restart and a fetch take.
fn job_id(result: &rhema_core::ToolResult) -> String {
    assert!(!result.is_error, "{}", result.content);
    let start = result
        .content
        .find("job-")
        .expect("a job id in the start result");
    let rest = &result.content[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// Waits until `id` leaves the status snapshot: a finished job leaves
/// it at once, its finish line is read from the feed after.
async fn wait_finished(handle: &rhema_subagents::SubagentHandle, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle.status().iter().all(|status| status.id != id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} never finished: {:?}",
            handle.status()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The child's one `turn` record, or the count of them when the
/// ledger holds more.
fn turn_record(session: &Session) -> rhema_submissions::Record {
    let mut found = snapshot(session)
        .unwrap()
        .iter()
        .filter(|record| record.kind == turn::kind())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        found.len(),
        1,
        "the child holds exactly one turn record: {found:?}"
    );
    found.remove(0)
}

#[tokio::test]
async fn a_transient_failure_then_a_restart_continues_the_same_child() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let provider = Arc::new(Outage {
        served: AtomicUsize::new(0),
    });
    let shell = Arc::new(ShellTools::new(dir.path()));
    // The inner tools execute the scripted `echo`, so the first run
    // lands a real tool result in the child before it dies.
    shell.install_approval(Arc::new(AllowAll));
    let tools = Arc::new(
        rhema_subagents::SubagentTools::new(provider, vec![def()], shell, "test-model")
            .with_retry_ladders(Vec::new(), Vec::new()),
    );
    tools.attach_store(store.clone(), parent.clone());
    let handle = tools.handle();
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        run_id: "turn-1".into(),
        ..ToolContext::default()
    };
    let start = ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({ "name": "worker", "prompt": "do the work" }),
            ..ToolCall::default()
        },
        &ctx,
    )
    .await;
    let id = job_id(&start);
    let child = handle
        .take_starts()
        .pop()
        .expect("the start named its child")
        .child;
    wait_finished(&handle, &id).await;
    let failed = handle.take_finished();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].line.contains("finished with an error"),
        "{}",
        failed[0].line
    );

    // What the failed run left behind: its turn settled retryable and
    // its committed step in the child, on disk.
    let settled = store.load(&child).unwrap().expect("the child was saved");
    assert!(matches!(
        turn_record(&settled).outcome,
        Some(Outcome::Failed {
            retryable: true,
            ..
        })
    ));
    assert!(
        settled.history.iter().any(|message| message
            .content
            .iter()
            .any(|content| matches!(content,
                Content::ToolResult(result) if result.content.contains("step-one")))),
        "the first run's tool result is committed: {:?}",
        settled.history
    );

    // The restart continues the same child, and it completes.
    let restarted = handle.restart_failed_jobs();
    assert_eq!(
        restarted,
        vec![rhema_subagents::Restarted {
            id: id.clone(),
            name: "worker".into(),
        }]
    );
    wait_finished(&handle, &id).await;
    assert_eq!(
        handle.take_finished()[0].line,
        "Subagent worker (job-1) has finished."
    );

    let continued = store.load(&child).unwrap().expect("the child was saved");
    // The failed run's work is intact: its tool result still rides the
    // history the restart continued.
    assert!(
        continued.history.iter().any(|message| message
            .content
            .iter()
            .any(|content| matches!(content,
                Content::ToolResult(result) if result.content.contains("step-one")))),
        "the spent history is kept: {:?}",
        continued.history
    );
    // The prompt was not duplicated: it is the one user message.
    assert_eq!(
        continued
            .history
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1,
        "{:?}",
        continued.history
    );
    // The turn was re-armed, not re-submitted: one record, done.
    let record = turn_record(&continued);
    assert_eq!(record.status, Status::Done);
    assert_eq!(
        kind::report(&continued).as_deref(),
        Some("the report after the outage")
    );
    // The report is the fetch's cache, and the parent adopts the same
    // child it always owned.
    let report = ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c2".into(),
            name: "subagent".into(),
            args: serde_json::json!({ "id": id }),
            ..ToolCall::default()
        },
        &ctx,
    )
    .await;
    assert!(!report.is_error, "{}", report.content);
    assert_eq!(report.content, "the report after the outage");
}

#[tokio::test]
async fn a_resumed_child_answers_its_dangling_call_with_the_verify_state_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let child = kind::child_id(store.as_ref(), &parent, 5).expect("a short parent mints a child");
    // A `Running` child whose tail is a dangling `bash` call: the shape
    // a kill mid-tool leaves, with the shell's own tool name.
    store
        .save(&child, &child_session(&child, &parent, Status::Running))
        .unwrap();

    let tools = rhema_subagents::SubagentTools::new(
        provider(),
        vec![def()],
        Arc::new(ShellTools::new(dir.path())),
        "test-model",
    );
    tools.attach_store(store.clone(), parent);
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        run_id: "turn-1".into(),
        ..ToolContext::default()
    };

    let report = tools.resume(&child, &ctx).await.expect("the tail resumes");
    assert_eq!(report, "the rest of the work");

    // The dangling call was revived, not merely closed: `bash` declares
    // no replay safety, so it lands the verify-state failure the
    // engine spells, the contract the parent's resume answers with.
    let resumed = store.load(&child).unwrap().expect("the child was saved");
    let results: Vec<&rhema_core::ToolResult> = resumed
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "the call is answered exactly once");
    assert_eq!(results[0].id, "c1");
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains("verify state before retrying"),
        "the revival's own wording, not the close's: {}",
        results[0].content
    );
}

#[tokio::test]
async fn a_first_run_starts_a_fresh_child() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(FileStore::new(dir.path()));
    let parent = parent_id();
    let tools = Arc::new(rhema_subagents::SubagentTools::new(
        provider(),
        vec![def()],
        Arc::new(ShellTools::new(dir.path())),
        "test-model",
    ));
    tools.attach_store(store.clone(), parent);
    let handle = tools.handle();
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        run_id: "turn-1".into(),
        ..ToolContext::default()
    };
    let start = ToolResolver::resolve(
        tools.as_ref(),
        ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({ "name": "worker", "prompt": "do the work" }),
            ..ToolCall::default()
        },
        &ctx,
    )
    .await;
    let id = job_id(&start);
    let child = handle
        .take_starts()
        .pop()
        .expect("the start named its child")
        .child;
    wait_finished(&handle, &id).await;

    // The minted child names nothing when it starts, so the first run
    // is the fresh one: the prompt, the answer, and nothing else.
    let session = store.load(&child).unwrap().expect("the child was saved");
    assert_eq!(session.history.len(), 2, "{:?}", session.history);
    assert_eq!(session.history[0].role, Role::User);
    assert!(matches!(
        session.history[0].content.first(),
        Some(Content::Text(text)) if text == "do the work"
    ));
    assert_eq!(session.history[1].role, Role::Assistant);
    assert_eq!(turn_record(&session).status, Status::Done);
}
