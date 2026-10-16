//! The falsification test (specs/rhema-memory.md §6, after SPEC.md §7
//! test 5's pattern): memory rides the existing seams. A scripted
//! provider saves a memory in one turn and searches for it in the next;
//! `run_turn` and `ToolResolver` carry the whole exchange. If wiring
//! memory into a turn ever needs anything new in `rhema-core`, the
//! boundary is wrong — fix the boundary, don't add the type.

use std::sync::Arc;

use rhema_core::{
    Agent, ChatParams, Content, Event, EventSink, Message, MockProvider, ModelOutput, Role,
    Session, StopReason, ToolCall, ToolContext, Usage, run_turn,
};
use rhema_memory::{MemoryQuery, MemoryResolver, MemoryStore, Scope, VolatileStore};

struct Quiet;

impl EventSink for Quiet {
    fn emit(&mut self, _event: Event<'_>) {}
}

fn scripted(id: &str, name: &str, args: serde_json::Value) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                args,
                provider_data: serde_json::Value::Null,
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn text(answer: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, answer),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

#[tokio::test]
async fn a_turn_can_save_and_recall_memory() {
    let store = Arc::new(VolatileStore::new());
    let resolver = Arc::new(MemoryResolver::new(store.clone(), Scope::Global));
    let provider = Arc::new(MockProvider::new(vec![
        scripted(
            "call-save",
            rhema_memory::MEMORY_SAVE,
            serde_json::json!({"body": "the release channel is #deploys", "tags": ["release"]}),
        ),
        scripted(
            "call-search",
            rhema_memory::MEMORY_SEARCH,
            serde_json::json!({"query": "release channel"}),
        ),
        text("recalled"),
    ]));

    let mut agent = Agent::builder()
        .provider(provider)
        .resolver(resolver)
        .params(ChatParams::new("mock"))
        .build()
        .unwrap();
    let mut session = Session::default();
    let ctx = ToolContext::default();
    let input = Message::text(Role::User, "remember the release channel, then recall it");

    let output = run_turn(&mut agent, &mut session, input, &mut Quiet, &ctx, 8)
        .await
        .unwrap();
    assert_eq!(output.stop_reason, StopReason::EndTurn);

    let result_for = |id: &str| {
        session
            .history
            .iter()
            .filter_map(|message| {
                message.content.iter().find_map(|content| match content {
                    Content::ToolResult(result) if result.id == id => Some(result),
                    _ => None,
                })
            })
            .next()
            .unwrap_or_else(|| panic!("no tool result for {id}"))
    };

    let saved = result_for("call-save");
    assert!(!saved.is_error, "{}", saved.content);
    assert!(saved.content.starts_with("saved "), "{}", saved.content);

    let recalled = result_for("call-search");
    assert!(!recalled.is_error, "{}", recalled.content);
    assert!(
        recalled.content.contains("the release channel is #deploys"),
        "{}",
        recalled.content
    );

    // And exactly one record now exists.
    let found = store.search(MemoryQuery::new("")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tags, vec!["release".to_owned()]);
}

#[tokio::test]
async fn a_failing_memory_call_stays_in_band() {
    let resolver = Arc::new(MemoryResolver::new(
        Arc::new(VolatileStore::new()),
        Scope::Global,
    ));
    let provider = Arc::new(MockProvider::new(vec![
        scripted(
            "call-bad",
            rhema_memory::MEMORY_SAVE,
            serde_json::json!({"id": "19700101T000000-00000009", "body": "ghost update"}),
        ),
        text("gave up"),
    ]));

    let mut agent = Agent::builder()
        .provider(provider)
        .resolver(resolver)
        .params(ChatParams::new("mock"))
        .build()
        .unwrap();
    let mut session = Session::default();
    let ctx = ToolContext::default();

    run_turn(
        &mut agent,
        &mut session,
        Message::text(Role::User, "update the ghost record"),
        &mut Quiet,
        &ctx,
        8,
    )
    .await
    .unwrap();

    let failed = session
        .history
        .iter()
        .filter_map(|message| {
            message.content.iter().find_map(|content| match content {
                Content::ToolResult(result) if result.id == "call-bad" => Some(result),
                _ => None,
            })
        })
        .next()
        .unwrap();
    assert!(failed.is_error, "{}", failed.content);
    assert!(
        failed.content.contains("no record with id"),
        "{}",
        failed.content
    );
}
