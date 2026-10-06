#![cfg(feature = "mock")]

use std::sync::Arc;

use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, MockProvider, ModelOutput, Role,
    Session, StopReason, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
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
        match call.name.as_str() {
            "echo" => ToolResult::success(call.id, call.args.to_string()),
            "fail" => ToolResult::failure(call.id, "resolver said no"),
            other => ToolResult::failure(call.id, format!("unknown tool: {other}")),
        }
    }
}

// Events borrow from the provider's output; keep owned copies for order
// assertions.
#[derive(Debug, Clone, PartialEq)]
enum Recorded {
    StepStart {
        iteration: u32,
    },
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta(String),
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    StepEnd {
        usage: Usage,
        stop_reason: StopReason,
    },
}

impl From<Event<'_>> for Recorded {
    fn from(event: Event<'_>) -> Self {
        match event {
            Event::StepStart { iteration } => Self::StepStart { iteration },
            Event::TextDelta(text) => Self::TextDelta(text.to_owned()),
            Event::ToolCallStart { id, name } => Self::ToolCallStart {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            Event::ToolCallArgsDelta(args) => Self::ToolCallArgsDelta(args.to_owned()),
            Event::ToolResult {
                id,
                content,
                is_error,
            } => Self::ToolResult {
                id: id.to_owned(),
                content: content.to_owned(),
                is_error,
            },
            Event::StepEnd { usage, stop_reason } => Self::StepEnd {
                usage,
                stop_reason: stop_reason.clone(),
            },
        }
    }
}

#[derive(Default)]
struct RecordSink {
    events: Vec<Recorded>,
}

impl EventSink for RecordSink {
    fn emit(&mut self, event: Event<'_>) {
        self.events.push(event.into());
    }
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
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

fn tool_result_of(message: &Message) -> &ToolResult {
    match &message.content[..] {
        [Content::ToolResult(result)] => result,
        other => panic!("expected a single tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn step_dispatches_the_tool_once_and_appends_the_result() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![
            tool_use_output(call("call_1", "echo", json!({ "text": "hi" }))),
            end_turn_output("all done"),
        ])))
        .resolver(Arc::new(EchoResolver))
        .params(ChatParams::new("mock-model"))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    let step = agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    assert_eq!(step.output.stop_reason, StopReason::ToolUse);
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[0].role, Role::Assistant);
    assert_eq!(
        session.history[0].content,
        vec![Content::ToolCall(call(
            "call_1",
            "echo",
            json!({ "text": "hi" })
        ))]
    );
    assert_eq!(session.history[1].role, Role::Tool);
    assert_eq!(
        tool_result_of(&session.history[1]),
        &ToolResult::success("call_1", r#"{"text":"hi"}"#)
    );
    // The provider announced the call, so the engine must not echo a second
    // ToolCallStart.
    assert_eq!(
        sink.events,
        vec![
            Recorded::StepStart { iteration: 0 },
            Recorded::ToolCallStart {
                id: "call_1".into(),
                name: "echo".into(),
            },
            Recorded::ToolCallArgsDelta(r#"{"text":"hi"}"#.into()),
            Recorded::ToolResult {
                id: "call_1".into(),
                content: r#"{"text":"hi"}"#.into(),
                is_error: false,
            },
            Recorded::StepEnd {
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
            },
        ]
    );

    sink.events.clear();
    let step = agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    assert_eq!(step.output.stop_reason, StopReason::EndTurn);
    assert_eq!(session.history.len(), 3);
    assert_eq!(
        session.history[2],
        Message::text(Role::Assistant, "all done")
    );
    assert_eq!(
        sink.events,
        vec![
            Recorded::StepStart { iteration: 1 },
            Recorded::TextDelta("all done".into()),
            Recorded::StepEnd {
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            },
        ]
    );
}

#[tokio::test]
async fn a_failing_tool_appends_an_error_result_and_keeps_the_step_ok() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![tool_use_output(call(
            "call_1",
            "fail",
            json!({}),
        ))])))
        .resolver(Arc::new(EchoResolver))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    let step = agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    assert_eq!(step.output.stop_reason, StopReason::ToolUse);
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[1].role, Role::Tool);
    let result = tool_result_of(&session.history[1]);
    assert!(result.is_error);
    assert_eq!(result.content, "resolver said no");
}

#[tokio::test]
async fn a_tool_use_without_a_resolver_appends_an_error_result() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![tool_use_output(call(
            "call_1",
            "echo",
            json!({}),
        ))])))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    let step = agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    assert_eq!(step.output.stop_reason, StopReason::ToolUse);
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[1].role, Role::Tool);
    let result = tool_result_of(&session.history[1]);
    assert!(result.is_error);
    assert_eq!(result.content, "no tool resolver is configured");
}

#[tokio::test]
async fn an_unknown_tool_name_is_an_in_band_error() {
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![tool_use_output(call(
            "call_1",
            "nope",
            json!({}),
        ))])))
        .resolver(Arc::new(EchoResolver))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    let step = agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    assert_eq!(step.output.stop_reason, StopReason::ToolUse);
    assert_eq!(session.history.len(), 2);
    let result = tool_result_of(&session.history[1]);
    assert!(result.is_error);
    assert_eq!(result.content, "unknown tool: nope");
    assert_eq!(
        sink.events[3],
        Recorded::ToolResult {
            id: "call_1".into(),
            content: "unknown tool: nope".into(),
            is_error: true,
        }
    );
}
