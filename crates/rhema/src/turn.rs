//! The tokio tasks: turn execution and model-list fetches. Everything here
//! runs OFF the UI thread and reports back through the UI event channel.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use rhema_coding_agent::{Approval, CodingAgent};
use rhema_core::{Content, Event, EventSink, Message, Role, Session, ToolResult};
#[cfg(feature = "mock")]
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::events::{FetchWhy, OwnedEvent, UiEvent};

/// A sink that forwards every engine event to the UI loop.
struct ChannelSink {
    tx: UnboundedSender<UiEvent>,
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: Event<'_>) {
        // A closed channel means the UI is gone; drop the event rather
        // than aborting the stream mid-turn.
        let _ = self.tx.send(UiEvent::Engine(OwnedEvent::from(&event)));
    }
}

/// Resolves once the cancel flag flips; a dropped sender (the UI is gone
/// or the turn already ended) means never — the turn's own completion is
/// what finishes the select below then.
async fn cancelled(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Appends failure results for tool calls a cancelled turn left hanging.
/// A provider rejects an assistant tool call with no matching result, so
/// without this the next turn would fail before saying a word.
fn repair_dangling_calls(session: &mut Session) {
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

/// Spawns one turn on a tokio task: runs `agent.turn` with the given
/// approval hook, checkpoints the session on success (when `save` is set),
/// and hands the agent back via [`UiEvent::TurnFinished`] — or, when the
/// cancel flag flips, drops the turn future at its current await and hands
/// the agent back via [`UiEvent::TurnCancelled`]. The UI thread must treat
/// the agent as gone until either event arrives.
pub fn spawn_turn(
    mut agent: CodingAgent,
    input: String,
    approval: Arc<dyn Approval>,
    save: Option<PathBuf>,
    cancel: watch::Receiver<bool>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sink = ChannelSink { tx: events.clone() };
        // The select's scope ends before the match: the pinned future —
        // and with it the borrow of `agent` — is gone by then.
        let result = {
            let turn = agent.turn(input, &mut sink, approval);
            tokio::pin!(turn);
            tokio::select! {
                _ = cancelled(cancel) => None,
                result = &mut turn => Some(result),
            }
        };
        match result {
            Some(result) => {
                // Checkpoint only after a completed turn, so the file never
                // contains a half-finished (tool-call without result) history.
                if result.is_ok() {
                    if let Some(path) = &save {
                        let _ = agent.save_session_to(path);
                    }
                }
                let _ = events.send(UiEvent::TurnFinished {
                    agent: Box::new(agent),
                    result: result.map_err(|err| err.to_string()),
                });
            }
            None => {
                // The drop can land between a committed tool call and its
                // result; the history must stay load-bearing for the next
                // turn.
                repair_dangling_calls(agent.session_mut());
                let _ = events.send(UiEvent::TurnCancelled {
                    agent: Box::new(agent),
                });
            }
        }
    })
}

/// Spawns the model-list fetch for `why` on a tokio task. The fetch doubles
/// as login validation: a wrong key or a dead server yields the provider's
/// error text.
pub fn spawn_model_fetch(
    backend: crate::app::Backend,
    why: FetchWhy,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = backend.list_models().await;
        let _ = events.send(UiEvent::ModelsLoaded { why, result });
    })
}

/// Drains `events` until a turn finishes, applying engine events to
/// `transcript`; used by tests to exercise a whole turn offline.
#[cfg(feature = "mock")]
pub async fn drive_turn_for_test(
    events: &mut UnboundedReceiver<UiEvent>,
    transcript: &mut crate::transcript::Transcript,
) -> (Box<CodingAgent>, Result<rhema_core::ModelOutput, String>) {
    loop {
        match events.recv().await.expect("turn task still alive") {
            UiEvent::Engine(owned) => transcript.apply(&owned),
            UiEvent::TurnFinished { agent, result } => return (agent, result),
            UiEvent::TurnCancelled { .. }
            | UiEvent::ModelsLoaded { .. }
            | UiEvent::ApprovalRequested { .. } => {}
        }
    }
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests {
    use super::*;
    use crate::transcript::{Line, Transcript};
    use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig};
    use rhema_core::{
        Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role, StopReason, Usage,
    };
    use tokio::sync::mpsc::unbounded_channel;

    fn scripted_output(text: &str) -> ModelOutput {
        ModelOutput {
            message: Message::text(Role::Assistant, text),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                cached_input_tokens: 1,
            },
        }
    }

    fn watch_cancel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    /// A provider that never answers: the turn parks in the model stream,
    /// exactly where a keyboard cancel must land.
    struct HangingProvider;

    #[async_trait::async_trait]
    impl ModelProvider for HangingProvider {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            std::future::pending().await
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_streams_into_the_transcript_and_returns_the_agent() {
        let (tx, mut rx) = unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let agent = CodingAgent::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output(
                "hello agent",
            )])),
            CodingAgentConfig::new("test-model", std::env::temp_dir()),
        )
        .unwrap();
        spawn_turn(agent, "hi".into(), Arc::new(AllowAll), None, cancelled, tx);

        let mut transcript = Transcript::new();
        let (agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok());
        // The user prompt and the assistant reply landed in the history.
        assert_eq!(agent.session().history.len(), 2);
        // The user line is pushed app-side at submit time; the turn task
        // only contributes the streamed assistant text.
        assert_eq!(transcript.lines(), &[Line::Assistant("hello agent".into())]);
        drop(cancel);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_turn_checkpoints_the_session() {
        let (tx, mut rx) = unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let agent = CodingAgent::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output("done")])),
            CodingAgentConfig::new("test-model", std::env::temp_dir()),
        )
        .unwrap();
        spawn_turn(
            agent,
            "hi".into(),
            Arc::new(AllowAll),
            Some(save.clone()),
            cancelled,
            tx,
        );
        let mut transcript = Transcript::new();
        let (_agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok());
        let raw = std::fs::read_to_string(&save).unwrap();
        assert!(raw.contains("\"history\""), "checkpoint written");
        drop(cancel);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_mid_stream_returns_the_agent_with_the_prompt_only() {
        let (tx, mut rx) = unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let agent = CodingAgent::new(
            Arc::new(HangingProvider),
            CodingAgentConfig::new("test-model", std::env::temp_dir()),
        )
        .unwrap();
        spawn_turn(agent, "hi".into(), Arc::new(AllowAll), None, cancelled, tx);

        cancel.send(true).unwrap();
        loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::TurnCancelled { agent } => {
                    // At most the prompt committed — zero when the flag
                    // won the race against the task's first poll. Either
                    // way the history is clean for the next turn.
                    assert!(agent.session().history.len() <= 1);
                    return;
                }
                UiEvent::TurnFinished { .. } => {
                    panic!("a cancelled turn must not come back as finished");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn repair_closes_tool_calls_a_drop_left_hanging() {
        use rhema_core::ToolCall;

        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "run_command".into(),
            args: serde_json::json!({ "command": "x" }),
            provider_data: serde_json::Value::Null,
        };
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
