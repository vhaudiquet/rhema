//! Zero-network demo: a scripted `MockProvider` drives `run_turn` through
//! one tool call and a final answer, printing every event as it streams.
//!
//! Run with `cargo run -p rhema-core --features mock --example step_demo`.

use std::sync::Arc;

use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, MockProvider, ModelOutput, Role,
    Session, StopReason, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec, Usage,
    run_turn,
};
use serde_json::json;

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

struct PrintSink;

impl EventSink for PrintSink {
    fn emit(&mut self, event: Event<'_>) {
        println!("{event:?}");
    }
}

// The workspace tokio has no rt-multi-thread; a current-thread runtime is
// enough for a scripted, zero-network demo.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let provider = Arc::new(MockProvider::new(vec![
        ModelOutput {
            message: Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "echo".into(),
                    args: json!({ "text": "hello, rhema" }),
                })],
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 11,
                output_tokens: 8,
                cached_input_tokens: 0,
            },
        },
        ModelOutput {
            message: Message::text(Role::Assistant, "echo says: hello, rhema"),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 29,
                output_tokens: 6,
                cached_input_tokens: 11,
            },
        },
    ]));

    let mut agent = Agent::builder()
        .provider(provider)
        .resolver(Arc::new(EchoResolver))
        .system("you echo things")
        .params(ChatParams::new("mock-model"))
        .build()
        .expect("agent with a provider builds");

    let mut session = Session::default();
    let mut sink = PrintSink;
    let out = run_turn(
        &mut agent,
        &mut session,
        Message::text(Role::User, "say hello through the echo tool"),
        &mut sink,
        &ToolContext::default(),
        4,
    )
    .await
    .expect("scripted turn completes");

    for content in &out.message.content {
        if let Content::Text(text) = content {
            println!("final: {text}");
        }
    }
    println!(
        "usage: {} in / {} out / {} cached",
        out.usage.input_tokens, out.usage.output_tokens, out.usage.cached_input_tokens
    );
}
