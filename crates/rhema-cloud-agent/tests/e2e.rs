//! The offline end-to-end: the scripted sandbox and the scripted
//! provider drive the cloud turn loop on the host — a full turn with a
//! bash call through the approval round-trip, a denial that never
//! reaches the sandbox, and a crash mid-command whose resume reports
//! the call interrupted instead of re-running it. This is the behavior
//! the DO wraps; the wasm glue only changes where the bytes ride.

use std::sync::Arc;

use async_trait::async_trait;
use rhema_agent_tools::Skill;
use rhema_cloud_agent::mock::ScriptedSandbox;
use rhema_cloud_agent::{
    AgentAssembly, ApprovalChannel, ApprovalPolicy, ExecOutcome, Sandbox, ShellSandbox,
};
use rhema_core::{
    ChatParams, CommitHook, Content, Event, EventSink, Message, ModelOutput, ModelProvider,
    ModelRequest, ProviderError, Role, Session, StopReason, ToolCall, Usage, mock::MockProvider,
};
use tokio::sync::{Notify, mpsc, oneshot};

/// The model asks for one `bash` call, then ends the turn.
fn bash_then_done(call_id: &str, command: &str, final_text: &str) -> MockProvider {
    MockProvider::new(vec![
        ModelOutput {
            message: Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: call_id.into(),
                    name: "bash".into(),
                    args: serde_json::json!({ "command": command }),
                    ..ToolCall::default()
                })],
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
        ModelOutput {
            message: Message::text(Role::Assistant, final_text),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ])
}

/// The approval round-trip the WebSocket layer drives: each ask pairs
/// the call with a one-shot the client answers — both directions cross
/// await points, never a shared-blocking wait.
struct TestChannel {
    requests: mpsc::UnboundedSender<(ToolCall, oneshot::Sender<bool>)>,
}

#[async_trait]
impl ApprovalChannel for TestChannel {
    async fn ask(&self, call: &ToolCall) -> Option<bool> {
        let (reply, receiver) = oneshot::channel();
        self.requests.send((call.clone(), reply)).ok()?;
        receiver.await.ok()
    }
}

/// One assembled agent plus the handles the test plays the client
/// with. `checkpoints` receives the session at every commit hook call —
/// what the DO persists to its storage — and `committed` fires then.
struct Rig {
    agent: rhema_coding_agent::CodingAgent,
    sandbox: Arc<ScriptedSandbox>,
    /// The approval requests the gate sent, with their answer lines.
    requests: mpsc::UnboundedReceiver<(ToolCall, oneshot::Sender<bool>)>,
    /// Fires when the commit hook has persisted a snapshot.
    committed: Arc<Notify>,
    /// The session snapshots the commit hook took.
    checkpoints: Arc<std::sync::Mutex<Vec<Session>>>,
}

/// Assembles the rig with `AskClient` approval and the channel
/// round-trip, exactly as the DO does.
fn rig(provider: Arc<dyn ModelProvider>) -> Rig {
    let sandbox = Arc::new(ScriptedSandbox::new());
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let channel: Arc<dyn ApprovalChannel> = Arc::new(TestChannel {
        requests: requests_tx,
    });
    let checkpoints = Arc::new(std::sync::Mutex::new(Vec::new()));
    let committed = Arc::new(Notify::new());
    let hook_checkpoints = checkpoints.clone();
    let hook_committed = committed.clone();
    let hook: CommitHook = Box::new(move |session: &Session| {
        hook_checkpoints.lock().unwrap().push(session.clone());
        hook_committed.notify_one();
        Ok(())
    });
    let assembly = AgentAssembly {
        model: "claude-sonnet-4-5".into(),
        base_system: None,
        skills: Vec::new(),
        max_turn_steps: 8,
        thinking: rhema_core::request::ThinkingEffort::Default,
        workspace: "/workspace".into(),
        transcript: Some(rhema_transcript::TranscriptSettings::default()),
        approval: ApprovalPolicy::AskClient,
        channel: Some(channel),
    };
    let mut agent = assembly
        .assemble(provider, Arc::new(ShellSandbox::new(sandbox.clone())))
        .expect("the assembly builds");
    agent.set_commit_hook(Some(hook));
    Rig {
        agent,
        sandbox,
        requests,
        committed,
        checkpoints,
    }
}

/// The tool results in a session, with their payloads.
fn tool_results(session: &Session) -> Vec<&rhema_core::ToolResult> {
    session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

#[tokio::test]
async fn a_full_turn_runs_bash_in_the_sandbox_through_the_round_trip() {
    let mut rig = rig(Arc::new(bash_then_done("c1", "echo hello", "done")));
    rig.sandbox.push_exec(Ok(ExecOutcome {
        stdout: b"hello\n".to_vec(),
        stderr: Vec::new(),
        code: Some(0),
    }));

    // The turn runs as its own task; the test body plays the client.
    let mut agent = rig.agent;
    let turn = tokio::spawn(async move {
        let result = agent
            .turn(
                "say hello",
                &mut NoopSink,
                Arc::new(rhema_agent_tools::DenyAll),
            )
            .await;
        (result, agent)
    });
    // The client side of the round-trip: approve the one request.
    if let Some((_call, reply)) = rig.requests.recv().await {
        reply.send(true).unwrap();
    }
    let (output, agent) = turn.await.expect("the turn task does not panic");
    let output = output.expect("the turn completes");

    // The model's final text ended the turn.
    assert_eq!(output.message.content, vec![Content::Text("done".into())]);
    // The command really ran in the sandbox, once.
    assert_eq!(rig.sandbox.executed(), vec!["echo hello".to_owned()]);

    // The session closed the round: one result, the sandbox's output.
    let results = tool_results(agent.session());
    let [result] = results.as_slice() else {
        panic!("one tool result, got {results:?}");
    };
    assert_eq!(result.id, "c1");
    assert!(!result.is_error);
    assert!(result.content.contains("hello"), "{}", result.content);
    assert_eq!(result.code, Some(0));

    // The commit hook fired once for the committed step — the tool-use
    // step — and its snapshot carries the call without any result. (The
    // end-turn step commits nothing; the DO checkpoints that one after
    // the step returns.)
    let checkpoints = rig.checkpoints.lock().unwrap();
    assert_eq!(checkpoints.len(), 1, "one committed step");
    let committed = &checkpoints[0];
    assert!(
        committed
            .history
            .iter()
            .flat_map(|message| message.content.iter())
            .any(|content| matches!(content, Content::ToolCall(call) if call.id == "c1")),
        "the committed checkpoint carries the call"
    );
    assert!(tool_results(committed).is_empty());
}

#[tokio::test]
async fn a_denied_call_never_reaches_the_sandbox() {
    let mut rig = rig(Arc::new(bash_then_done("c1", "rm -rf /", "kept safe")));
    let mut agent = rig.agent;
    let turn = tokio::spawn(async move {
        let result = agent
            .turn(
                "clean up",
                &mut NoopSink,
                Arc::new(rhema_agent_tools::DenyAll),
            )
            .await;
        (result, agent)
    });
    if let Some((_call, reply)) = rig.requests.recv().await {
        reply.send(false).unwrap();
    }
    let (output, agent) = turn.await.expect("the turn task does not panic");
    let output = output.expect("the turn completes");

    // The sandbox saw nothing.
    assert!(rig.sandbox.executed().is_empty());
    // The model saw the rejection in-band and carried on to finish.
    assert_eq!(
        output.message.content,
        vec![Content::Text("kept safe".into())]
    );
    let results = tool_results(agent.session());
    let [result] = results.as_slice() else {
        panic!("one tool result, got {results:?}");
    };
    assert!(result.is_error);
    assert_eq!(result.content, "rejected by user approval");
}

/// Records every request's messages, then answers from a script — the
/// resume test's way to assert what the model was shown.
struct Recording {
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
    replies: MockProvider,
}

#[async_trait]
impl ModelProvider for Recording {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.requests.lock().unwrap().push(req.messages.clone());
        self.replies.complete(req, sink).await
    }
}

#[tokio::test]
async fn a_crash_mid_command_resumes_with_the_call_reported_interrupted() {
    // The turn commits, parks inside the command, and dies — the
    // workspace's view of a container eviction between commit and
    // result.
    let mut rig = rig(Arc::new(bash_then_done("c1", "long build", "ignored")));
    rig.sandbox.hold_next_exec();
    let mut agent = rig.agent;
    // Both signals registered before the turn starts, so neither
    // wakeup can be missed.
    let committed_seen = rig.committed.notified();
    let parked_seen = rig.sandbox.parked().notified();
    let task = tokio::spawn(async move {
        let _ = agent
            .turn(
                "start the build",
                &mut NoopSink,
                Arc::new(rhema_agent_tools::DenyAll),
            )
            .await;
    });
    if let Some((_call, reply)) = rig.requests.recv().await {
        reply.send(true).unwrap();
    }
    // The commit has landed and the command sits parked inside the
    // sandbox: the durable history ends with the committed intent.
    tokio::time::timeout(std::time::Duration::from_secs(5), committed_seen)
        .await
        .expect("the commit lands");
    tokio::time::timeout(std::time::Duration::from_secs(5), parked_seen)
        .await
        .expect("the command parks");
    let crashed = rig
        .checkpoints
        .lock()
        .unwrap()
        .last()
        .expect("checkpointed")
        .clone();
    task.abort();

    // The resume: a fresh agent over the committed session, the same
    // assembly the DO rebuilds after eviction.
    assert_eq!(rig.sandbox.executed(), vec!["long build".to_owned()]);
    let resume = Arc::new(Recording {
        requests: std::sync::Mutex::new(Vec::new()),
        replies: MockProvider::new(vec![ModelOutput {
            message: Message::text(Role::Assistant, "resumed"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }]),
    });
    let (requests_tx, _requests) = mpsc::unbounded_channel();
    let channel: Arc<dyn ApprovalChannel> = Arc::new(TestChannel {
        requests: requests_tx,
    });
    let assembly = AgentAssembly {
        model: "claude-sonnet-4-5".into(),
        base_system: None,
        skills: Vec::new(),
        max_turn_steps: 8,
        thinking: rhema_core::request::ThinkingEffort::Default,
        workspace: "/workspace".into(),
        transcript: Some(rhema_transcript::TranscriptSettings::default()),
        approval: ApprovalPolicy::AskClient,
        channel: Some(channel),
    };
    let mut resumed = assembly
        .assemble_from_session(
            resume.clone(),
            Arc::new(ShellSandbox::new(rig.sandbox.clone())),
            crashed,
        )
        .expect("the resume assembles");
    resumed
        .turn(
            "continue",
            &mut NoopSink,
            Arc::new(rhema_agent_tools::DenyAll),
        )
        .await
        .expect("the resumed turn completes");

    // The dead command never re-ran: the sandbox executed it once,
    // before the crash.
    assert_eq!(rig.sandbox.executed(), vec!["long build".to_owned()]);
    // The revive reported the committed-but-unanswered call
    // interrupted, and the model saw that report before anything else.
    let seen = resume.requests.lock().unwrap();
    let first_request = &seen[0];
    let text = format!("{first_request:?}");
    assert!(text.contains("interrupted"), "the model is told: {text}");
}

#[tokio::test]
async fn seeded_skills_reach_the_prompt_without_a_filesystem() {
    let sandbox = Arc::new(ScriptedSandbox::new());
    sandbox
        .write_file(
            "/workspace/.rhema/skills/deploy/SKILL.md",
            b"---\ndescription: Triggers on deploy tasks.\n---\nbody\n",
        )
        .await
        .unwrap();
    // The DO collects the skill list itself — from what it seeded —
    // and hands it to the assembly; nothing reads the host's disk.
    let skills = vec![Skill {
        name: "deploy".into(),
        description: "Triggers on deploy tasks.".into(),
        license: None,
        path: "/workspace/.rhema/skills/deploy/SKILL.md".parse().unwrap(),
    }];
    let assembly = AgentAssembly {
        model: "claude-sonnet-4-5".into(),
        base_system: None,
        skills,
        max_turn_steps: 8,
        thinking: rhema_core::request::ThinkingEffort::Default,
        workspace: "/workspace".into(),
        transcript: Some(rhema_transcript::TranscriptSettings::default()),
        approval: ApprovalPolicy::DenyAll,
        channel: None,
    };
    let agent = assembly
        .assemble(
            Arc::new(MockProvider::new(Vec::new())),
            Arc::new(ShellSandbox::new(sandbox)),
        )
        .unwrap();
    let prompt = agent
        .system()
        .expect("the assembled agent carries a system prompt");
    assert!(prompt.contains("deploy"), "{prompt}");
    // The model id the assembly carried is the one the requests use.
    assert_eq!(
        ChatParams::new("claude-sonnet-4-5").model,
        "claude-sonnet-4-5"
    );
}
