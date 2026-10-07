//! The tokio tasks: turn execution and model-list fetches. Everything here
//! runs OFF the UI thread and reports back through the UI event channel.

use std::path::PathBuf;
use std::sync::Arc;

use rhema_coding_agent::{Approval, CodingAgent};
use rhema_core::{Event, EventSink};
#[cfg(feature = "mock")]
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
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

/// Spawns one turn on a tokio task: runs `agent.turn` with the given
/// approval hook, checkpoints the session on success (when `save` is set),
/// and hands the agent back via [`UiEvent::TurnFinished`]. The UI thread
/// must treat the agent as gone until that event arrives.
pub fn spawn_turn(
    mut agent: CodingAgent,
    input: String,
    approval: Arc<dyn Approval>,
    save: Option<PathBuf>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sink = ChannelSink { tx: events.clone() };
        let result = agent.turn(input, &mut sink, approval).await;
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
            UiEvent::ModelsLoaded { .. } | UiEvent::ApprovalRequested { .. } => {}
        }
    }
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests {
    use super::*;
    use crate::transcript::{Line, Transcript};
    use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig};
    use rhema_core::{Message, ModelOutput, Role, StopReason, Usage};
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_streams_into_the_transcript_and_returns_the_agent() {
        let (tx, mut rx) = unbounded_channel();
        let agent = CodingAgent::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output(
                "hello agent",
            )])),
            CodingAgentConfig::new("test-model", std::env::temp_dir()),
        )
        .unwrap();
        spawn_turn(agent, "hi".into(), Arc::new(AllowAll), None, tx);

        let mut transcript = Transcript::new();
        let (agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok());
        // The user prompt and the assistant reply landed in the history.
        assert_eq!(agent.session().history.len(), 2);
        // The user line is pushed app-side at submit time; the turn task
        // only contributes the streamed assistant text.
        assert_eq!(transcript.lines(), &[Line::Assistant("hello agent".into())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_turn_checkpoints_the_session() {
        let (tx, mut rx) = unbounded_channel();
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
            tx,
        );
        let mut transcript = Transcript::new();
        let (_agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok());
        let raw = std::fs::read_to_string(&save).unwrap();
        assert!(raw.contains("\"history\""), "checkpoint written");
    }
}
