//! Integration tests for rhema-core.
#![cfg(feature = "mock")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rhema_core::{
    Agent, AgentError, ChatParams, Content, Event, EventSink, Message, MockProvider, ModelOutput,
    ModelProvider, ModelRequest, ProviderError, Role, Session, Step, StopReason, ToolCall,
    ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
};
use serde_json::{Value, json};

struct RecordingProvider {
    inner: Arc<MockProvider>,
    requests: Mutex<Vec<(Role, String)>>,
}

// The recording wraps a MockProvider, showing the provider seam takes any
// impl; the request log is what proves the next step sees the inbox item.
#[rhema_core::async_trait]
impl ModelProvider for RecordingProvider {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let (role, text) = match req.messages.last() {
            Some(message) => (
                message.role,
                message
                    .content
                    .iter()
                    .find_map(|c| match c {
                        Content::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            ),
            None => (Role::System, String::new()),
        };
        self.requests.lock().unwrap().push((role, text));
        self.inner.complete(req, sink).await
    }
}

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

// The crate-2 dispatcher shape — acquire, step, poll_inbox, checkpoint,
// release — built only on the public seams. If this needed a new type in
// crate 1, the boundary would be wrong (SPEC.md §1, §7 test 5).
struct FakeRsdDriver {
    agent: Agent,
    session: Session,
    inbox: VecDeque<Message>,
}

impl FakeRsdDriver {
    async fn step(
        &mut self,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
    ) -> Result<Step, AgentError> {
        self.agent.step(&mut self.session, sink, ctx).await
    }

    fn poll_inbox(&mut self) {
        if let Some(item) = self.inbox.pop_front() {
            self.session.history.push(item);
        }
    }

    fn checkpoint(&mut self) {
        let before = self.session.clone();
        let json = serde_json::to_string(&self.session).expect("session serializes");
        self.session = serde_json::from_str(&json).expect("session deserializes");
        assert_eq!(
            self.session, before,
            "checkpoint must preserve the session exactly"
        );
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

#[tokio::test]
async fn an_rsd_style_driver_runs_across_checkpoints_and_inbox_items() {
    let recorder = Arc::new(RecordingProvider {
        inner: Arc::new(MockProvider::new(vec![
            tool_use_output(call("call_1", "echo", json!({ "text": "summarizing" }))),
            end_turn_output("done; the note is acknowledged"),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let agent = Agent::builder()
        .provider(recorder.clone())
        .resolver(Arc::new(EchoResolver))
        .params(ChatParams::new("mock-model"))
        .build()
        .unwrap();
    let mut driver = FakeRsdDriver {
        agent,
        session: Session {
            history: Vec::new(),
            driver_state: json!({ "key": "src/main.rs", "revision": 7 }),
            cost_usd: 0.0,
            usage_stats: serde_json::Value::Null,
        },
        inbox: VecDeque::from([Message::text(Role::User, "inbox: the build turned red")]),
    };
    driver
        .session
        .history
        .push(Message::text(Role::User, "summarize this module"));
    let mut sink = NullSink;
    let ctx = ToolContext::default();

    driver.checkpoint();
    driver.step(&mut sink, &ctx).await.unwrap();
    driver.checkpoint();
    driver.poll_inbox();
    driver.checkpoint();

    let step = driver.step(&mut sink, &ctx).await.unwrap();
    assert_eq!(step.output.stop_reason, StopReason::EndTurn);

    let requests = recorder.requests.lock().unwrap();
    assert_eq!(
        *requests,
        vec![
            (Role::User, "summarize this module".into()),
            (Role::User, "inbox: the build turned red".into()),
        ]
    );
    assert_eq!(
        driver.session.driver_state,
        json!({ "key": "src/main.rs", "revision": 7 })
    );
    let roles: Vec<Role> = driver.session.history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::Assistant,
        ]
    );
}
