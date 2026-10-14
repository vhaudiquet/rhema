//! Dangling-call repair: closing tool calls a crash or a cancel left
//! hanging, so the history stays load-bearing for the next turn.
//!
//! A provider rejects an assistant tool call with no matching result, so
//! without this the next turn would fail before saying a word. Two paths:
//! [`repair_dangling_calls`] for the cancel path — a cancelled call never
//! re-runs, whatever its declared safety — and [`revive_dangling_calls`]
//! for the crash-resume path, which re-executes calls whose tool declared
//! [`ReplaySafety::Safe`] and reports the rest interrupted.

use std::collections::HashSet;

use rhema_core::{
    Content, Message, ReplaySafety, Role, Session, ToolCall, ToolContext, ToolResolver, ToolResult,
};

/// Appends failure results for tool calls a dropped turn left hanging.
/// A provider rejects an assistant tool call with no matching result, so
/// without this the next turn would fail before saying a word. The drop
/// can land between a committed call and its result — cancellation by
/// future-drop (the TUI) and by `session/cancel` (ACP) both unwind this
/// way — and the history must stay load-bearing for the next turn.
pub fn repair_dangling_calls(session: &mut Session) {
    let answered: HashSet<String> = session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult(result) => Some(result.id.clone()),
            _ => None,
        })
        .collect();
    let dangling: Vec<String> = session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolCall(call) if !answered.contains(&call.id) => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    for id in dangling {
        session.history.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult::failure(
                id,
                "cancelled by user",
            ))],
        });
    }
}

/// Closes calls a crash left committed-but-unanswered, with replay
/// awareness: a call whose tool declared [`ReplaySafety::Safe`] is
/// re-executed now against `resolver` so the round completes; an unsafe
/// one gets an in-band failure telling the model the call was interrupted
/// before its result landed, so it can verify state before retrying. One
/// `Tool` result per dangling call, in call order; a clean history is
/// untouched. The cancel path uses [`repair_dangling_calls`] instead — a
/// cancelled call never re-runs, whatever its declared safety.
pub async fn revive_dangling_calls(
    session: &mut Session,
    resolver: &dyn ToolResolver,
    ctx: &ToolContext,
) {
    let answered: HashSet<String> = session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolResult(result) => Some(result.id.clone()),
            _ => None,
        })
        .collect();
    let dangling: Vec<ToolCall> = session
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::ToolCall(call) if !answered.contains(&call.id) => Some(call.clone()),
            _ => None,
        })
        .collect();
    for call in dangling {
        let result = match resolver.replay_safety(&call.name) {
            ReplaySafety::Safe => resolver.resolve(call, ctx).await,
            ReplaySafety::Unsafe => ToolResult::failure(
                &call.id,
                "interrupted: the call was committed but its result never \
                 landed; its effect may have partially applied — verify \
                 state before retrying",
            ),
        };
        session.history.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(result)],
        });
    }
}

#[cfg(test)]
mod repair_tests {
    use super::*;
    use rhema_core::ToolCall;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: json!({ "command": "x" }),
            provider_data: serde_json::Value::Null,
        }
    }

    #[test]
    fn repair_closes_tool_calls_a_drop_left_hanging() {
        // The drop landed after call-a's result: call-b hangs.
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "go"));
        session.history.push(Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("on it".into()),
                Content::ToolCall(call("call-a")),
                Content::ToolCall(call("call-b")),
            ],
        });
        session.history.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult::success("call-a", "ok"))],
        });

        repair_dangling_calls(&mut session);
        let last = session.history.last().unwrap();
        match last.content.first() {
            Some(Content::ToolResult(result)) => {
                assert_eq!(result.id, "call-b");
                assert!(result.is_error, "the repair is a failure result");
            }
            other => panic!("expected a repaired tool result, got {other:?}"),
        }
        // A repaired history is closed: nothing dangles.
        assert_eq!(session.history.len(), 4);
        // And it never over-repairs.
        repair_dangling_calls(&mut session);
        assert_eq!(session.history.len(), 4);
    }
}

#[cfg(test)]
mod revive_tests {
    use super::*;
    use rhema_core::ToolSpec;
    use serde_json::json;

    /// Two tools: `safe_tool` declares Safe, `unsafe_tool` keeps the
    /// conservative default.
    struct Mixed;

    #[async_trait::async_trait]
    impl ToolResolver for Mixed {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![
                ToolSpec::new("safe_tool", "safe to re-run", json!({})),
                ToolSpec::new("unsafe_tool", "must not re-run", json!({})),
            ]
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success(call.id, "re-ran")
        }

        fn replay_safety(&self, name: &str) -> ReplaySafety {
            match name {
                "safe_tool" => ReplaySafety::Safe,
                _ => ReplaySafety::Unsafe,
            }
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args: json!({}),
            provider_data: serde_json::Value::Null,
        }
    }

    fn session_with_dangling() -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "go"));
        session.history.push(Message {
            role: Role::Assistant,
            content: vec![
                Content::ToolCall(call("safe_1", "safe_tool")),
                Content::ToolCall(call("unsafe_1", "unsafe_tool")),
            ],
        });
        session
    }

    #[tokio::test]
    async fn revive_reexecutes_safe_calls_and_reports_unsafe_ones() {
        let mut session = session_with_dangling();

        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;

        assert_eq!(session.history.len(), 4);
        let results: Vec<&ToolResult> = session.history[2..]
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                Content::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        // Call order preserved: the safe call's fresh result first.
        assert_eq!(results[0].id, "safe_1");
        assert!(!results[0].is_error);
        assert_eq!(results[0].content, "re-ran");
        assert_eq!(results[1].id, "unsafe_1");
        assert!(results[1].is_error);
        assert!(results[1].content.contains("interrupted"));
        // A revived history is closed: nothing dangles.
        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;
        assert_eq!(session.history.len(), 4);
    }

    #[tokio::test]
    async fn revive_leaves_a_clean_history_alone() {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "go"));
        session.history.push(Message::text(Role::Assistant, "done"));

        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;

        assert_eq!(session.history.len(), 2);
    }
}
