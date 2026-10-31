//! Integration tests for rhema-core.
#![cfg(feature = "mock")]

use std::sync::Arc;

use rhema_core::{
    Agent, AgentError, ChatParams, CommitError, Content, Event, EventSink, Message, MockProvider,
    ModelOutput, Role, Session, StopReason, ToolCall, ToolContext, ToolResolver, ToolResult,
    ToolSpec, Usage,
};
use serde_json::{Value, json};
use std::sync::Mutex;

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
    ThinkingDelta(String),
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        delta: String,
    },
    ToolOutputDelta {
        id: String,
        chunk: String,
    },
    ToolStatus {
        id: String,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
        code: Option<i32>,
    },
    /// The running usage tally; the mock never emits one.
    Usage(Usage),
    StepEnd {
        usage: Usage,
        stop_reason: StopReason,
    },
    /// A retry wait; the mock never emits one.
    RetryWait,
}

impl From<Event<'_>> for Recorded {
    fn from(event: Event<'_>) -> Self {
        match event {
            Event::StepStart { iteration } => Self::StepStart { iteration },
            Event::ThinkingDelta(text) => Self::ThinkingDelta(text.to_owned()),
            Event::TextDelta(text) => Self::TextDelta(text.to_owned()),
            Event::ToolCallStart { id, name } => Self::ToolCallStart {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            Event::ToolCallArgsDelta { id, delta } => Self::ToolCallArgsDelta {
                id: id.to_owned(),
                delta: delta.to_owned(),
            },
            Event::ToolStatus { id, .. } => Self::ToolStatus { id: id.to_owned() },
            Event::ToolOutputDelta { id, chunk } => Self::ToolOutputDelta {
                id: id.to_owned(),
                chunk: chunk.to_owned(),
            },
            Event::ToolResult {
                id,
                content,
                is_error,
                code,
            } => Self::ToolResult {
                id: id.to_owned(),
                content: content.to_owned(),
                is_error,
                code,
            },
            Event::Usage(usage) => Self::Usage(usage),
            Event::RetryWait { .. } => Self::RetryWait,
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
            Recorded::ToolCallArgsDelta {
                id: "call_1".into(),
                delta: r#"{"text":"hi"}"#.into(),
            },
            Recorded::ToolResult {
                id: "call_1".into(),
                content: r#"{"text":"hi"}"#.into(),
                is_error: false,
                code: None,
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
async fn a_streaming_tool_rides_the_sink_as_deltas_before_its_result() {
    /// Streams two progress lines, then answers.
    struct Streaming;

    #[rhema_core::async_trait]
    impl ToolResolver for Streaming {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec::new("slow", "streams its work", json!({}))]
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success(call.id, "done")
        }

        async fn resolve_streaming(
            &self,
            call: ToolCall,
            _ctx: &ToolContext,
            progress: &mut dyn rhema_core::ToolProgress,
        ) -> ToolResult {
            progress.output("working\n");
            progress.output("still working\n");
            ToolResult::success(call.id, "done")
        }
    }

    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![tool_use_output(call(
            "call_1",
            "slow",
            json!({}),
        ))])))
        .resolver(Arc::new(Streaming))
        .build()
        .unwrap();
    let mut session = Session::default();
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    agent.step(&mut session, &mut sink, &ctx).await.unwrap();

    // The engine stamps the call's id on the progress lines, and the
    // result follows them — never the other way around. (StepStart,
    // ToolCallStart, and the mock's args delta come first.)
    assert_eq!(
        sink.events[3..6],
        [
            Recorded::ToolOutputDelta {
                id: "call_1".into(),
                chunk: "working\n".into(),
            },
            Recorded::ToolOutputDelta {
                id: "call_1".into(),
                chunk: "still working\n".into(),
            },
            Recorded::ToolResult {
                id: "call_1".into(),
                content: "done".into(),
                is_error: false,
                code: None,
            },
        ]
    );
    // Progress is ephemeral: the session carries only the result.
    assert_eq!(session.history.len(), 2);
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
            code: None,
        }
    );
}

/// A shared log of the stages a step passed through, so a hook closure and
/// a resolver record into one order.
#[derive(Clone, Default)]
struct StageLog(Arc<Mutex<Vec<String>>>);

impl StageLog {
    fn push(&self, stage: String) {
        self.0.lock().unwrap().push(stage);
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A resolver that logs each resolve and answers.
struct LoggingResolver {
    log: StageLog,
}

#[rhema_core::async_trait]
impl ToolResolver for LoggingResolver {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "echo",
            "echoes its arguments back",
            json!({}),
        )]
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        self.log.push("resolve".into());
        ToolResult::success(call.id, "done")
    }
}

#[tokio::test]
async fn the_commit_hook_fires_at_every_stream_boundary() {
    let log = StageLog::default();
    let hook_log = log.clone();
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![
            tool_use_output(call("call_1", "echo", json!({}))),
            end_turn_output("all done"),
        ])))
        .resolver(Arc::new(LoggingResolver { log: log.clone() }))
        .commit_hook(Box::new(move |session: &Session| {
            // The hook sees the session as it stands at each boundary,
            // with the role of whatever landed last.
            let role = session
                .history
                .last()
                .map(|message| format!("{:?}", message.role))
                .unwrap_or_else(|| "empty".into());
            hook_log.push(format!("commit:{}:{role}", session.history.len()));
            Ok(())
        }))
        .build()
        .unwrap();
    let mut session = Session::default();
    session.history.push(Message::text(Role::User, "go"));
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    agent.step(&mut session, &mut sink, &ctx).await.unwrap();
    assert_eq!(
        log.entries(),
        vec![
            // The input was durable before the request spent tokens on
            // it: the hook saw the caller's message alone.
            "commit:1:User".to_owned(),
            // Commit-before-execute held too: the hook saw exactly the
            // assistant message, history len 2, before any tool result
            // existed.
            "commit:2:Assistant".to_owned(),
            "resolve".to_owned(),
            // The stream's outcome landed: the result is in the
            // snapshot the step's last fire took.
            "commit:3:Tool".to_owned(),
        ]
    );

    // A text-only step commits at its two boundaries as well: a turn
    // that never touches a tool still leaves every stream on disk.
    agent.step(&mut session, &mut sink, &ctx).await.unwrap();
    assert_eq!(
        log.entries(),
        vec![
            "commit:1:User".to_owned(),
            "commit:2:Assistant".to_owned(),
            "resolve".to_owned(),
            "commit:3:Tool".to_owned(),
            // The second stream's input changed nothing since the last
            // commit; the hook fires all the same.
            "commit:3:Tool".to_owned(),
            "commit:4:Assistant".to_owned(),
        ]
    );
}

#[tokio::test]
async fn a_failing_commit_before_a_tool_fails_the_step_with_nothing_run() {
    let log = StageLog::default();
    let mut agent = Agent::builder()
        .provider(Arc::new(MockProvider::new(vec![tool_use_output(call(
            "call_1",
            "echo",
            json!({}),
        ))])))
        .resolver(Arc::new(LoggingResolver { log: log.clone() }))
        .commit_hook(Box::new(|session: &Session| {
            // The boundaries decide one by one: the stream's input
            // commits, the call intent — the fire that guards side
            // effects — fails.
            if session.history.len() >= 2 {
                Err(CommitError("disk full".into()))
            } else {
                Ok(())
            }
        }))
        .build()
        .unwrap();
    let mut session = Session::default();
    session.history.push(Message::text(Role::User, "go"));
    let mut sink = RecordSink::default();
    let ctx = ToolContext::default();

    let err = agent.step(&mut session, &mut sink, &ctx).await.unwrap_err();

    assert_eq!(err, AgentError::Commit(CommitError("disk full".into())));
    // The intent stays in the history; no result followed it, and the
    // resolver never ran.
    assert_eq!(session.history.len(), 2);
    assert_eq!(session.history[1].role, Role::Assistant);
    assert!(log.entries().is_empty());
}
