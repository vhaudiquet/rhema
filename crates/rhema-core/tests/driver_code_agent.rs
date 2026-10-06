#![cfg(feature = "mock")]

use std::sync::Arc;

use rhema_core::{
    Agent, AgentError, Content, Event, EventSink, Message, MockProvider, ModelOutput, Role,
    Session, StopReason, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
    run_turn,
};
use serde_json::{Value, json};

struct EchoResolver;

#[rhema_core::async_trait]
impl ToolResolver for EchoResolver {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "echo",
            "echoes its arguments back",
            json!({ "type": "object" }),
        )]
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success(call.id, call.args.to_string())
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

fn call(id: impl Into<String>, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        args,
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
async fn run_turn_drives_tool_use_to_a_final_answer() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![
            tool_use_output(call("call_1", "echo", json!({ "text": "hi" }))),
            end_turn_output("all done"),
        ])))
        .resolver(Arc::new(EchoResolver))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = NullSink;
    let ctx = ToolContext::default();

    let out = run_turn(
        &mut agent,
        &mut session,
        Message::text(Role::User, "run the echo tool"),
        &mut sink,
        &ctx,
        5,
    )
    .await
    .unwrap();

    assert_eq!(out.stop_reason, StopReason::EndTurn);
    assert_eq!(out.message, Message::text(Role::Assistant, "all done"));
    let roles: Vec<Role> = session.history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(
        session.history[0],
        Message::text(Role::User, "run the echo tool")
    );
    assert_eq!(
        session.history[3],
        Message::text(Role::Assistant, "all done")
    );
}

#[tokio::test]
async fn run_turn_is_bounded_by_max_iterations() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new((0..3).map(|i| {
            tool_use_output(call(format!("call_{i}"), "echo", json!({})))
        }))))
        .resolver(Arc::new(EchoResolver))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = NullSink;
    let ctx = ToolContext::default();

    let err = run_turn(
        &mut agent,
        &mut session,
        Message::text(Role::User, "loop forever"),
        &mut sink,
        &ctx,
        3,
    )
    .await
    .unwrap_err();

    assert_eq!(err, AgentError::MaxIterations(3));
    let roles: Vec<Role> = session.history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::Tool,
        ]
    );
}
