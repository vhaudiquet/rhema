//! The tokio tasks: turn execution and model-list fetches. Everything here
//! runs OFF the UI thread and reports back through the UI event channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rhema_coding_agent::{Approval, CodingAgent, repair_dangling_calls};
use rhema_core::{Event, EventSink, Session};
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate};
#[cfg(feature = "mock")]
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::events::{FetchWhy, OwnedEvent, UiEvent};

/// The wall-clock bound on one backend's catalog fetch, DNS included:
/// name resolution sits outside the adapters' request timeouts, so one
/// unreachable instance (a `.lan` server that is off) must not hold
/// the picker's whole answer past this.
const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Spawns one turn on a tokio task: runs `agent.turn` with the given
/// approval hook, commits the session before each tool round and
/// checkpoints it on success (when `save` is set), and hands the agent
/// back via [`UiEvent::TurnFinished`] — or, when the cancel flag flips,
/// drops the turn future at its current await and hands the agent back
/// via [`UiEvent::TurnCancelled`]. The UI thread must treat the agent as
/// gone until either event arrives.
pub fn spawn_turn(
    mut agent: CodingAgent,
    input: String,
    approval: Arc<dyn Approval>,
    save: Option<PathBuf>,
    cancel: watch::Receiver<bool>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    // Commit-before-execute: when the session persists to a file, the
    // engine hands it over after the assistant message lands and before
    // the first tool runs — a crash mid-call leaves a committed intent
    // (the next turn's revive closes it) instead of no record at all.
    if let Some(path) = save.clone() {
        agent.set_commit_hook(Some(Box::new(move |session: &Session| {
            crate::sessions::write_session(&path, session).map_err(|err| err.to_string())
        })));
    }
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
                // Checkpoint after a completed turn: the file's freshest
                // complete state. A crash can leave a committed intent (a
                // call without its result) in the file — the next turn's
                // revive closes exactly that, so it is a resume input now,
                // not a half-finished write.
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
    backends: Vec<crate::app::Candidate>,
    why: FetchWhy,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // One task per backend, gathered as they land: a slow catalog
        // cannot delay the others, and each reports its own failure so a
        // dead provider never blanks the picker the live ones filled.
        let mut set = tokio::task::JoinSet::new();
        for candidate in backends {
            set.spawn(async move {
                let provider = candidate.backend.config_name();
                let result = match tokio::time::timeout(
                    CATALOG_FETCH_TIMEOUT,
                    candidate.backend.list_models(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "no answer within {} s — is the server up?",
                        CATALOG_FETCH_TIMEOUT.as_secs()
                    )),
                };
                crate::events::CatalogFetch {
                    provider,
                    instance: candidate.instance,
                    result,
                }
            });
        }
        let mut result = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(entry) = joined {
                result.push(entry);
            }
        }
        result.sort_by_key(|entry| entry.provider);
        let _ = events.send(UiEvent::ModelsLoaded { why, result });
    })
}

/// Spawns the subscription-allowance fetch on a tokio task. The result
/// is tagged with the backend's config name, so a report of a backend
/// the session has since switched away from never shows.
pub fn spawn_usage_fetch(
    backend: crate::app::Backend,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let provider = backend.config_name();
        let result = backend.subscription_usage().await;
        let _ = events.send(UiEvent::UsageLoaded { provider, result });
    })
}

/// Spawns the interactive login flow, forwarding its updates as
/// [`UiEvent::LoginUpdate`]. The task ends when the flow does; a change
/// on `cancelled` (the Esc path's `true`) or a dropped sender (the
/// app's cleanup after the credential lands) ends it early.
pub fn spawn_login_flow(
    flow: Box<dyn InteractiveLogin>,
    mut cancelled: watch::Receiver<bool>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sink = FlowSink(events);
        tokio::select! {
            _ = cancelled.changed() => {}
            _ = flow.run(&mut sink) => {}
        }
    })
}

/// Forwards login-flow updates onto the UI channel; a closed channel
/// means the UI is gone, and the flow dies with the task.
struct FlowSink(UnboundedSender<UiEvent>);

impl LoginSink for FlowSink {
    fn update(&mut self, update: LoginUpdate) {
        let _ = self.0.send(UiEvent::LoginUpdate(update));
    }
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
            | UiEvent::UsageLoaded { .. }
            | UiEvent::ApprovalRequested { .. }
            | UiEvent::LoginUpdate(_) => {}
        }
    }
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests {
    use super::*;
    use crate::transcript::{Line, Transcript};
    use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig};
    use rhema_core::{
        Content, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
        StopReason, ToolCall, Usage,
    };
    use serde_json::json;
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

    /// The model calls the real `bash` tool with a long sleep: the cancel
    /// lands *inside* the tool call, after the commit hook fired.
    fn sleeping_tool_output() -> ModelOutput {
        tool_use_output(ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            args: json!({ "command": "sleep 30" }),
            provider_data: serde_json::Value::Null,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_inside_a_tool_leaves_the_committed_intent_in_the_file() {
        let (tx, mut rx) = unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let agent = CodingAgent::new(
            Arc::new(rhema_core::MockProvider::new(vec![sleeping_tool_output()])),
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

        // The hook writes the intent before the tool hangs; wait for the
        // file so the cancel cannot race ahead of the commit.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !save.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the commit hook never wrote the intent");

        cancel.send(true).unwrap();
        loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::TurnCancelled { .. } => {
                    // The file holds the committed intent: the user input
                    // and the assistant call, no result — exactly what a
                    // resumed session's revive closes.
                    let raw = std::fs::read_to_string(&save).unwrap();
                    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
                    let history = doc["history"].as_array().unwrap();
                    assert_eq!(history.len(), 2);
                    let last = serde_json::to_string(&history[1]).unwrap();
                    assert!(last.contains("call_1"), "intent committed: {last}");
                    assert!(!last.contains("\"is_error\""), "no result yet: {last}");
                    return;
                }
                UiEvent::TurnFinished { .. } => {
                    panic!("a hanging tool must not finish");
                }
                _ => {}
            }
        }
    }
}
