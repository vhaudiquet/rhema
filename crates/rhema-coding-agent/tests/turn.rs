#![cfg(feature = "mock")]

//! Turn semantics through the scripted provider: history shape, session
//! continuation, the step budget, and persistence/resume.

use std::sync::Arc;

use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig, CodingAgentError};
use rhema_core::{
    Content, Event, EventSink, Message, MockProvider, ModelOutput, Role, Session, StopReason,
    ToolCall, Usage,
};
use serde_json::json;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,

        provider_data: serde_json::Value::Null,
    }
}

fn tool_use_output(call: ToolCall) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(call)],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn end_turn_output(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

#[tokio::test]
async fn a_turn_writes_the_file_and_keeps_the_history_shape() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        tool_use_output(tool_call(
            "call_1",
            "bash",
            json!({ "command": "printf hello > notes.txt" }),
        )),
        end_turn_output("wrote notes.txt"),
    ]));
    let mut agent =
        CodingAgent::new(provider, CodingAgentConfig::new("mock-model", root.path())).unwrap();
    let mut sink = NullSink;

    let output = agent
        .turn("create notes.txt", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    assert_eq!(output.stop_reason, StopReason::EndTurn);
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "hello"
    );
    let roles: Vec<_> = agent.session().history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(
        agent.session().history[3],
        Message::text(Role::Assistant, "wrote notes.txt")
    );
}

#[tokio::test]
async fn a_second_turn_continues_the_same_session() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        tool_use_output(tool_call(
            "call_1",
            "bash",
            json!({ "command": "printf a > a.txt" }),
        )),
        end_turn_output("done one"),
        end_turn_output("done two"),
    ]));
    let mut agent =
        CodingAgent::new(provider, CodingAgentConfig::new("mock-model", root.path())).unwrap();
    let mut sink = NullSink;

    agent
        .turn("first", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();
    agent
        .turn("second", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    assert_eq!(agent.session().history.len(), 6);
    assert_eq!(
        agent.session().history[4],
        Message::text(Role::User, "second")
    );
    assert_eq!(
        agent.session().history[5],
        Message::text(Role::Assistant, "done two")
    );
}

#[tokio::test]
async fn a_turn_past_the_step_budget_fails_with_max_iterations() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        tool_use_output(tool_call("call_1", "list_dir", json!({}))),
        tool_use_output(tool_call("call_2", "list_dir", json!({}))),
    ]));
    let config = CodingAgentConfig {
        max_turn_steps: 2,
        ..CodingAgentConfig::new("mock-model", root.path())
    };
    let mut agent = CodingAgent::new(provider, config).unwrap();
    let mut sink = NullSink;

    let err = agent
        .turn("loop", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        CodingAgentError::Agent(rhema_core::AgentError::MaxIterations(2))
    );
}

#[tokio::test]
async fn session_round_trips_through_disk_and_resumes() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        end_turn_output("first answer"),
        end_turn_output("second answer"),
    ]));
    let mut agent = CodingAgent::new(
        provider.clone(),
        CodingAgentConfig::new("mock-model", root.path()),
    )
    .unwrap();
    let mut sink = NullSink;
    agent
        .turn("one", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    let path = root.path().join("session.json");
    agent.save_session_to(&path).unwrap();
    let loaded = CodingAgent::load_session_from(&path).unwrap();
    assert_eq!(&loaded, agent.session());

    let mut resumed = CodingAgent::from_session(
        provider,
        CodingAgentConfig::new("mock-model", root.path()),
        loaded,
    )
    .unwrap();
    assert_eq!(resumed.session().history.len(), 2);
    resumed
        .turn("two", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();
    assert_eq!(resumed.session().history.len(), 4);
    assert_eq!(
        resumed.session().history[0],
        Message::text(Role::User, "one")
    );
    assert_eq!(
        resumed.session().history[2],
        Message::text(Role::User, "two")
    );
}

#[tokio::test]
async fn a_saved_session_round_trips_as_pretty_json() {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![end_turn_output("hi")]));
    let mut agent =
        CodingAgent::new(provider, CodingAgentConfig::new("mock-model", root.path())).unwrap();
    let mut sink = NullSink;
    agent
        .turn("hello", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    let path = root.path().join("session.json");
    agent.save_session_to(&path).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    // Pretty JSON: multi-line, indented.
    assert!(raw.contains("\n  "));
    let back: Session = serde_json::from_str(&raw).unwrap();
    assert_eq!(back, *agent.session());
}
