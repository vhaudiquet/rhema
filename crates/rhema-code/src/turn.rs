//! The tokio tasks: turn execution and model-list fetches. Everything here
//! runs OFF the UI thread and reports back through the UI event channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rhema_coding_agent::{Approval, CodingAgent, repair_dangling_calls};
use rhema_core::{Event, EventSink, Image, Message, Role, Session};
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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

/// The steered message channel, read forever: a closed channel — the UI
/// dropped the sender mid-chain, so nothing more can arrive — parks this
/// future instead of resolving `None`, so the select's steer arm goes
/// silent rather than spinning on an immediately-ready receive.
async fn next_steer(steer: &mut UnboundedReceiver<String>) -> Option<String> {
    loop {
        match steer.recv().await {
            Some(text) => return Some(text),
            None => std::future::pending().await,
        }
    }
}

/// The prompt a turn chain starts from: the text and the images beside
/// it, in submission order. Every later prompt of the chain — a steered
/// nudge — is text only.
#[derive(Debug, Default)]
pub struct Prompt {
    /// The prompt's text.
    pub text: String,
    /// The images beside the text.
    pub images: Vec<Image>,
}

impl Prompt {
    /// A text-only prompt — every steered nudge's shape.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Spawns a turn chain on a tokio task: runs the prompt through the
/// agent with the given approval hook, checkpoints the session at every
/// stream boundary and again after the final turn (when `save` is set),
/// and hands the agent back via [`UiEvent::TurnFinished`] — or, when the
/// cancel flag flips, drops the turn future at its current await and
/// hands the agent back via [`UiEvent::TurnCancelled`]. A
/// message on `steer` drops the turn future the same way, but the task
/// carries on: the message runs as the next turn on the same agent, so
/// the conversation continues right where the interruption landed. The
/// UI thread must treat the agent as gone until either event arrives;
/// `steer`'s receiver travels inside that event, so a message sent in
/// the finishing race is buffered there, never lost.
pub fn spawn_turn(
    mut agent: CodingAgent,
    prompt: Prompt,
    approval: Arc<dyn Approval>,
    save: Option<PathBuf>,
    cancel: watch::Receiver<bool>,
    mut steer: UnboundedReceiver<String>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    // Commit at every stream boundary: when the session persists to a
    // file, the engine hands it over before each request (the prompt
    // durable before tokens are spent), after the assistant message
    // lands and before the first tool runs (a crash mid-call leaves a
    // committed intent — the next turn's revive closes it), and after
    // each step's results — a death mid-turn, mid-first-stream
    // included, leaves a resumable file instead of no record at all.
    if let Some(path) = save.clone() {
        agent.set_commit_hook(Some(Box::new(move |session: &Session| {
            crate::sessions::write_session(&path, session)
                .map_err(|err| rhema_core::CommitError(err.to_string()))
        })));
    }
    // The cancel flag the executor polls is an atomic, the UI's is a
    // watch: a mirror task keeps the two honest — a flip asks in-flight
    // tools to stop, not only drops the turn at its next await. The
    // task ends when the watch does (the turn chain finished or the UI
    // is gone).
    let cancelled_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mirror = Arc::clone(&cancelled_flag);
    let mut cancel_mirror = cancel.clone();
    tokio::spawn(async move {
        loop {
            if *cancel_mirror.borrow_and_update() {
                mirror.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            if cancel_mirror.changed().await.is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        let mut sink = ChannelSink { tx: events.clone() };
        let Prompt { text, images } = prompt;
        // The images ride the first turn only: a steered nudge is a
        // sentence, and `text_with_images` with none folds to the plain
        // text message.
        let mut text = text;
        let mut images = images;
        // The chain's outcome: `None` once the cancel flag ended it, the
        // final turn's result otherwise. A steered interruption leaves it
        // alone — the chain just turns another round. An intermediate
        // turn's error is carried no further than this task: the steered
        // turn proceeds, and only the final result is reported.
        let mut outcome = None;
        loop {
            let message = Message::text_with_images(
                Role::User,
                std::mem::take(&mut text),
                std::mem::take(&mut images),
            );
            let turn = agent.turn_message_with_cancel(
                message,
                &mut sink,
                Arc::clone(&approval),
                Arc::clone(&cancelled_flag),
            );
            tokio::pin!(turn);
            tokio::select! {
                _ = cancelled(cancel.clone()) => break,
                Some(nudge) = next_steer(&mut steer) => {
                    // The pinned future drops here, at its current await
                    // — a streaming request or a running tool. The next
                    // turn revives whatever committed before the drop
                    // (see `revive_dangling_calls`), so the history
                    // stays load-bearing without a repair step here.
                    text = nudge;
                }
                result = &mut turn => match steer.try_recv() {
                    // A message that raced the completion steers the next
                    // turn instead of slipping into the closing window.
                    Ok(nudge) => text = nudge,
                    Err(_) => {
                        outcome = Some(result);
                        break;
                    }
                },
            }
        }
        match outcome {
            Some(result) => {
                // Checkpoint after a completed turn: the file's freshest
                // complete state. A crash can leave a committed intent (a
                // call without its result) in the file — the next turn's
                // revive closes exactly that, so it is a resume input now,
                // not a half-finished write.
                if result.is_ok()
                    && let Some(path) = &save
                {
                    let _ = agent.save_session_to(path);
                }
                let _ = events.send(UiEvent::TurnFinished {
                    agent: Box::new(agent),
                    result: result.map_err(|err| err.to_string()),
                    steer,
                });
            }
            None => {
                // The drop can land between a committed tool call and its
                // result; the history must stay load-bearing for the next
                // turn.
                repair_dangling_calls(agent.session_mut());
                let _ = events.send(UiEvent::TurnCancelled {
                    agent: Box::new(agent),
                    steer,
                });
            }
        }
    })
}

/// Spawns the sandbox assembly a picker choice asked for on a tokio
/// task: the shared `assemble` — container stand-up or connection,
/// workspace seeding included — runs off the UI thread and reports as
/// [`UiEvent::SandboxAssembled`]. A failure lands as the error text;
/// the session's current tools keep running either way.
pub fn spawn_sandbox_assemble(
    choice: crate::args::SandboxChoice,
    profile: rhema_config::Profile,
    workspace: PathBuf,
    secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
    base_system: Option<String>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = crate::sandbox::assemble(
            choice,
            &profile,
            &workspace,
            secrets.as_deref(),
            base_system.as_deref(),
        )
        .await
        .map_err(|err| err.to_string());
        let _ = events.send(UiEvent::SandboxAssembled { choice, result });
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
            UiEvent::TurnFinished { agent, result, .. } => return (agent, result),
            UiEvent::TurnCancelled { .. }
            | UiEvent::ModelsLoaded { .. }
            | UiEvent::UsageLoaded { .. }
            | UiEvent::ApprovalRequested { .. }
            | UiEvent::SandboxAssembled { .. }
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

    /// The first call signals the test and hangs — the generation a steer
    /// interrupts — and every later call answers, standing in for the turn
    /// the steer starts.
    struct HangsThenAnswers {
        /// Signalled when the first generation enters `complete`, so the
        /// test only steers a live generation (steering earlier is a
        /// scheduling coin-flip that can drop the first future unpolled).
        entered: tokio::sync::Notify,
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl ModelProvider for HangsThenAnswers {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                std::future::pending().await
            } else {
                Ok(scripted_output("steered answer"))
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_steered_message_interrupts_the_turn_and_runs_as_the_next_turn() {
        let (tx, mut rx) = unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let (steer_tx, steered) = unbounded_channel();
        let provider = Arc::new(HangsThenAnswers {
            entered: tokio::sync::Notify::new(),
            calls: std::sync::atomic::AtomicU32::new(0),
        });
        let agent = CodingAgent::new(
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            CodingAgentConfig::new("test-model", std::env::temp_dir()),
        )
        .unwrap();
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Arc::new(AllowAll),
            None,
            cancelled,
            steered,
            tx,
        );

        // Wait for the first generation to be in flight, then steer: the
        // message drops it and starts the next turn with itself as the
        // input.
        tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
            .await
            .expect("the first generation never started");
        steer_tx.send("look at the tests instead".into()).unwrap();
        let mut transcript = Transcript::new();
        let (agent, result) = loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::Engine(owned) => transcript.apply(&owned),
                UiEvent::TurnFinished { agent, result, .. } => break (agent, result),
                UiEvent::TurnCancelled { .. } => {
                    panic!("a steered chain must not come back as cancelled");
                }
                _ => {}
            }
        };
        assert!(result.is_ok());
        // The interrupted generation left nothing behind: the history is
        // the first prompt, the steered message, and the steered turn's
        // reply — consecutive user messages are valid.
        let texts: Vec<String> = agent
            .session()
            .history
            .iter()
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "hi".to_owned(),
                "look at the tests instead".to_owned(),
                "steered answer".to_owned(),
            ]
        );
        // This provider never sinks deltas, so the transcript stays
        // empty; the streamed-turns-separate line is the app's concern
        // (the user line between them), not this task's.
        assert!(transcript.lines().is_empty());
        drop(cancel);
        // The returned receiver is drained: nothing was lost to the race.
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
        let (_steer_tx, steered) = unbounded_channel();
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Arc::new(AllowAll),
            None,
            cancelled,
            steered,
            tx,
        );

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
        let (_steer_tx, steered) = unbounded_channel();
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Arc::new(AllowAll),
            Some(save.clone()),
            cancelled,
            steered,
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
        let (_steer_tx, steered) = unbounded_channel();
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Arc::new(AllowAll),
            None,
            cancelled,
            steered,
            tx,
        );

        cancel.send(true).unwrap();
        loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::TurnCancelled { agent, .. } => {
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
        let (_steer_tx, steered) = unbounded_channel();
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Arc::new(AllowAll),
            Some(save.clone()),
            cancelled,
            steered,
            tx,
        );

        // The hook writes the intent before the tool hangs; wait until
        // the intent is on disk so the cancel cannot race ahead of the
        // commit. (The per-stream commits create the file earlier still
        // — the prompt alone — so existence alone proves nothing.)
        tokio::time::timeout(Duration::from_secs(5), async {
            while !std::fs::read_to_string(&save)
                .map(|raw| raw.contains("call_1"))
                .unwrap_or(false)
            {
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
