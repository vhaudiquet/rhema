//! The tokio tasks: turn execution and model-list fetches. Everything here
//! runs OFF the UI thread and reports back through the UI event channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rhema_core::{Event, EventSink, Image, Message, ModelOutput, Role, Session};
use rhema_host_agent::{HostAgent, HostAgentError};
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::events::{FetchWhy, OwnedEvent, UiEvent};
use rhema_submissions::{Outcome, SubmissionId};

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
/// or the turn already ended) means never, the turn's own completion is
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

/// The steered message channel, read forever: a closed channel, the UI
/// dropped the sender mid-chain, so nothing more can arrive, parks this
/// future instead of resolving `None`, so the select's steer arm goes
/// silent rather than spinning on an immediately-ready receive.
async fn next_steer(steer: &mut Receiver<String>) -> Option<String> {
    loop {
        match steer.recv().await {
            Some(text) => return Some(text),
            None => std::future::pending().await,
        }
    }
}

/// What a turn chain starts from. Every later turn of the chain, a
/// steered nudge, is a text-only user message either way.
#[derive(Debug)]
pub enum Prompt {
    /// A fresh prompt: the text, the paste spans it records, and the
    /// images beside it, in submission order, become the session's next
    /// user message.
    User {
        /// The prompt's text, payloads in place.
        text: String,
        /// The images beside the text.
        images: Vec<Image>,
        /// The paste spans the message annotates, for the transcript's
        /// chips.
        spans: Vec<crate::paste::PasteSpan>,
    },
    /// The failed request again: no new message, the turn re-issues
    /// the step loop on the history as the failed turn left it (see
    /// `HostAgent::retry_turn_with_cancel`). `/retry` starts this.
    Retry,
}

impl Prompt {
    /// A text-only prompt, every steered nudge's shape.
    pub fn text(text: impl Into<String>) -> Self {
        Self::User {
            text: text.into(),
            images: Vec::new(),
            spans: Vec::new(),
        }
    }
}

/// Spawns a turn chain on a tokio task: runs the prompt through the
/// agent, checkpoints the session at every stream boundary and again
/// after the final turn (when `save` is set), and hands the agent back
/// via [`UiEvent::TurnFinished`], or, when the
/// cancel flag flips, drops the turn future at its current await and
/// hands the agent back via [`UiEvent::TurnCancelled`]. A
/// message on `steer` drops the turn future the same way, but the task
/// carries on: the message runs as the next turn on the same agent, so
/// the conversation continues right where the interruption landed. The
/// UI thread must treat the agent as gone until either event arrives;
/// `steer`'s receiver travels inside that event, so a message sent in
/// the finishing race is buffered there, never lost.
pub fn spawn_turn(
    mut agent: HostAgent,
    prompt: Prompt,
    save: Option<PathBuf>,
    submission: Option<SubmissionId>,
    cancel: watch::Receiver<bool>,
    mut steer: Receiver<String>,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    // Commit at every stream boundary: when the session persists to a
    // file, the engine hands it over before each request (the prompt
    // durable before tokens are spent), after the assistant message
    // lands and before the first tool runs (a crash mid-call leaves a
    // committed intent, the next turn's revive closes it), and after
    // each step's results, a death mid-turn, mid-first-stream
    // included, leaves a resumable file instead of no record at all.
    if let Some(path) = save.clone() {
        agent.set_commit_hook(Some(Box::new(move |session: &Session| {
            crate::sessions::write_session(&path, session)
                .map_err(|err| rhema_core::CommitError(err.to_string()))
        })));
    }
    // The cancel flag the executor polls is an atomic, the UI's is a
    // watch: a mirror task keeps the two honest, a flip asks in-flight
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
        // The images ride the first turn only: a steered nudge is a
        // sentence, and `text_with_images` with none folds to the plain
        // text message. A retry consumes no input at all; the flag
        // flips after its one round.
        let (mut text, mut images, mut spans, mut retry) = match prompt {
            Prompt::User {
                text,
                images,
                spans,
            } => (text, images, spans, false),
            Prompt::Retry => (String::new(), Vec::new(), Vec::new(), true),
        };
        // The chain's outcome: `None` once the cancel flag ended it, the
        // final turn's result otherwise. A steered interruption leaves it
        // alone, the chain just turns another round. An intermediate
        // turn's error is carried no further than this task: the steered
        // turn proceeds, and only the final result is reported.
        let mut outcome = None;
        loop {
            // The retry round and the message rounds run different
            // agent futures; the box gives them one type for the select
            // below (and keeps the task Send).
            let mut turn: Pin<
                Box<dyn Future<Output = Result<ModelOutput, HostAgentError>> + Send>,
            > = if retry {
                retry = false;
                Box::pin(agent.retry_turn_with_cancel(&mut sink, Arc::clone(&cancelled_flag)))
            } else {
                let mut message = Message::text_with_images(
                    Role::User,
                    std::mem::take(&mut text),
                    std::mem::take(&mut images),
                );
                crate::paste::annotate(&mut message, &std::mem::take(&mut spans));
                Box::pin(agent.turn_message_with_cancel(
                    message,
                    &mut sink,
                    Arc::clone(&cancelled_flag),
                ))
            };
            tokio::select! {
                _ = cancelled(cancel.clone()) => break,
                Some(nudge) = next_steer(&mut steer) => {
                    // The pinned future drops here, at its current await
                    //, a streaming request or a running tool. The next
                    // turn revives whatever committed before the drop
                    // (see `revive_dangling_calls`), so the history
                    // stays load-bearing without a repair step here.
                    text = nudge;
                }
                result = &mut turn => {
                    // A message that raced the completion stays in the
                    // channel: it rides `TurnFinished` back to the app,
                    // which queues it as the next prompt. Taking it here
                    // ran another whole turn inside this task and threw
                    // the finished turn's result away with it, so the app
                    // never saw that turn's usage, error, or stop reason.
                    outcome = Some(result);
                    break;
                },
            }
        }
        match outcome {
            Some(result) => {
                // Settle with the error still in hand. Every failed
                // turn in this frontend re-arms: `/retry` re-sends the
                // failed request as it stands, be it after a rate
                // limit, a rejected key, or a switched model, and the
                // offer that gates it (`App::retryable`) looks at the
                // error alone, never its class. The ledger's
                // `retryable` must say the same, or the record refuses
                // the re-arm. The transient/permanent split stays
                // where it belongs: `rhema-provider`'s auto-retry
                // ladder, not the ledger's manual one.
                let outcome = match &result {
                    Ok(_) => Outcome::Done,
                    Err(err) => Outcome::Failed {
                        error: err.to_string(),
                        // Subagent child turns keep the class verdict
                        // (their `retryable` decides whether `/retry`
                        // restarts the job), but this chain settles
                        // only the parent's turn.
                        retryable: true,
                    },
                };
                settle_submission(&mut agent, submission.as_ref(), outcome);
                // Checkpoint after the chain's end: the file's freshest
                // complete state, status included. A crash can leave a
                // committed intent (a call without its result) in the
                // file, the next turn's revive closes exactly that, so it
                // is a resume input now, not a half-finished write.
                if let Some(path) = &save {
                    let _ = crate::sessions::write_session(path, agent.session());
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
                rhema_core::close_orphaned_calls(&mut agent.session_mut().history);
                settle_submission(&mut agent, submission.as_ref(), Outcome::Cancelled);
                if let Some(path) = &save {
                    let _ = crate::sessions::write_session(path, agent.session());
                }
                let _ = events.send(UiEvent::TurnCancelled {
                    agent: Box::new(agent),
                    steer,
                });
            }
        }
    })
}

/// Settles the chain's record at its end, warning on stderr when the
/// transition is refused: the session is still the truth either way.
fn settle_submission(agent: &mut HostAgent, submission: Option<&SubmissionId>, outcome: Outcome) {
    let Some(id) = submission else {
        return;
    };
    let at = rhema_submissions::now();
    if let Err(err) = rhema_submissions::settle(agent.session_mut(), id, outcome, at) {
        eprintln!("rhema-code: warning: submission {id} was not settled: {err}");
    }
}

/// Spawns the sandbox assembly a picker choice asked for on a tokio
/// task: the shared `assemble`, container stand-up or connection,
/// workspace seeding included, runs off the UI thread and reports as
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
                    Ok(Ok(models)) => Ok(models),
                    // A transport timeout names the provider unavailable:
                    // the picker paints it red instead of listing it with
                    // the provider's own error.
                    Ok(Err(rhema_core::ProviderError::Timeout)) => {
                        Err(crate::events::CatalogFailure::Unavailable)
                    }
                    Ok(Err(err)) => Err(crate::events::CatalogFailure::Error(err.to_string())),
                    Err(_) => Err(crate::events::CatalogFailure::Unavailable),
                };
                crate::events::CatalogFetch {
                    provider,
                    instance: candidate.instance,
                    result,
                }
            });
        }
        // Each answer is reported the moment it lands: the picker draws
        // the rows it has while the rest are still out, so one dead host
        // cannot hold every other provider's catalog back for the fetch
        // budget.
        let mut remaining = set.len();
        while let Some(joined) = set.join_next().await {
            remaining -= 1;
            // A task that died without a slice still counts as answered:
            // otherwise the page would spin on a provider forever.
            let result = joined.into_iter().collect();
            let _ = events.send(UiEvent::ModelsLoaded {
                why: why.clone(),
                result,
                remaining,
            });
        }
    })
}

/// Spawns the `/resume` load on the blocking pool: the session file's
/// read, parse, and usage derivation are blocking work the UI thread
/// must not do. The parts land tagged with the path, so the apply
/// re-checks what it was asked to load.
pub fn spawn_session_resume(
    path: std::path::PathBuf,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let result = crate::app::App::load_resume_parts(&path).map_err(|err| err.to_string());
        let _ = events.send(UiEvent::SessionResumed { path, result });
    })
}

/// Spawns the `/resume` listing on the blocking pool: the listing reads
/// and parses every saved session in its scope for its opening prompt,
/// which can be hundreds of megabytes, and the picker paints before
/// that work starts. The rows land tagged with the request, so a
/// listing a newer open superseded drops.
pub fn spawn_session_listing(
    base: std::path::PathBuf,
    workspace: std::path::PathBuf,
    all: bool,
    request: u64,
    events: UnboundedSender<UiEvent>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let result = if all {
            crate::sessions::list_all_sessions(&base)
        } else {
            crate::sessions::list_sessions(&base, &workspace)
        }
        .map_err(|err| err.to_string());
        let _ = events.send(UiEvent::SessionsListed {
            request,
            all,
            result,
        });
    })
}

/// Spawns the clipboard read for the paste key on the blocking pool:
/// the connection round trip and any image's PNG re-encode are
/// blocking work a key handler must not run inline, a large
/// screenshot would freeze every redraw until it encoded. The grab
/// lands as a `UiEvent::ClipboardGrabbed`.
pub fn spawn_clipboard_grab(events: UnboundedSender<UiEvent>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let grab = crate::screens::clipboard::grab();
        let _ = events.send(UiEvent::ClipboardGrabbed { grab });
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

/// The wall-clock bound on one update check: the fetch tools carry
/// shorter bounds of their own; this one keeps the `checking…` state
/// honest even when a transfer trickles past them.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawns a version check on a tokio task: the distribution
/// manifest's fetch and parse run off the UI thread and report as
/// [`UiEvent::UpdateChecked`]. The startup check, `/update` and the
/// Update page all spawn through here.
pub fn spawn_update_check(events: UnboundedSender<UiEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result =
            match tokio::time::timeout(UPDATE_CHECK_TIMEOUT, crate::update::fetch_latest_version())
                .await
            {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "no answer within {} s",
                    UPDATE_CHECK_TIMEOUT.as_secs()
                )),
            };
        let _ = events.send(UiEvent::UpdateChecked { result });
    })
}

/// Spawns an install run on a tokio task: the published
/// install script, a download of the whole release behind it, runs
/// off the UI thread with no time bound and reports as
/// [`UiEvent::UpdateInstalled`]. The running binary stays the old one
/// until a restart either way.
pub fn spawn_update_install(events: UnboundedSender<UiEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = crate::update::run_installer().await;
        let _ = events.send(UiEvent::UpdateInstalled { result });
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
    events: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    transcript: &mut crate::transcript::Transcript,
) -> (Box<HostAgent>, Result<rhema_core::ModelOutput, String>) {
    loop {
        match events.recv().await.expect("turn task still alive") {
            UiEvent::Engine(owned) => transcript.apply(&owned),
            UiEvent::TurnFinished { agent, result, .. } => return (agent, result),
            UiEvent::TurnCancelled { .. }
            | UiEvent::ModelsLoaded { .. }
            | UiEvent::UsageLoaded { .. }
            | UiEvent::ApprovalRequested { .. }
            | UiEvent::SandboxAssembled { .. }
            | UiEvent::SessionsListed { .. }
            | UiEvent::SessionResumed { .. }
            | UiEvent::ClipboardGrabbed { .. }
            | UiEvent::UpdateChecked { .. }
            | UiEvent::UpdateInstalled { .. }
            | UiEvent::LoginUpdate(_) => {}
        }
    }
}

#[cfg(all(test, feature = "mock"))]
mod mock_tests {
    use super::*;
    use crate::transcript::{Line, Transcript};
    use rhema_core::{
        Content, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
        StopReason, ToolCall, Usage,
    };
    use rhema_host_agent::{HostAgent, HostAgentConfig};
    use serde_json::json;

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
                annotations: Default::default(),
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

    /// Queues and starts a turn record the way the UI thread does before
    /// it spawns the chain.
    fn start_submission(agent: &mut HostAgent, input: &str) -> SubmissionId {
        let at = 1;
        let record = rhema_submissions::turn::submit(agent.session_mut(), input, at).unwrap();
        rhema_submissions::turn::start(agent.session_mut(), &record.id, input, at).unwrap();
        record.id
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

    /// The first call signals the test and hangs, the generation a steer
    /// interrupts, and every later call answers, standing in for the turn
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let (steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        let provider = Arc::new(HangsThenAnswers {
            entered: tokio::sync::Notify::new(),
            calls: std::sync::atomic::AtomicU32::new(0),
        });
        let mut agent = HostAgent::new(HostAgentConfig::new(
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let id = start_submission(&mut agent, "hi");
        spawn_turn(
            agent,
            Prompt::text("hi"),
            None,
            Some(id.clone()),
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
        steer_tx
            .try_send("look at the tests instead".into())
            .unwrap();
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
        // The chain is one prompt record, not one per steered round.
        let snapshot = rhema_submissions::snapshot(agent.session()).unwrap();
        assert_eq!(snapshot.len(), 1, "one record for the whole chain");
        assert_eq!(
            snapshot.get(&id).unwrap().status,
            rhema_submissions::Status::Done
        );
        // The interrupted generation left nothing behind: the history is
        // the first prompt, the steered message, and the steered turn's
        // reply, consecutive user messages are valid.
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
    async fn a_completed_chain_settles_done_and_saves_the_answer() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let mut agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output(
                "hello agent",
            )])),
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let id = start_submission(&mut agent, "hi");
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            Some(id.clone()),
            cancelled,
            steered,
            tx,
        );
        let mut transcript = Transcript::new();
        let (_agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok());

        let saved = HostAgent::load_session_from(&save).unwrap();
        let snapshot = rhema_submissions::snapshot(&saved).unwrap();
        let record = snapshot.get(&id).expect("the chain's record survived");
        assert_eq!(record.status, rhema_submissions::Status::Done);
        assert_eq!(
            rhema_submissions::turn::answer(&saved, &snapshot, &id).as_deref(),
            Some("hello agent")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_chain_settles_retryable_and_saves() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let mut agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(FailsThenSucceeds::default()) as Arc<dyn ModelProvider>,
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let id = start_submission(&mut agent, "hi");
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            Some(id.clone()),
            cancelled,
            steered,
            tx,
        );
        let mut transcript = Transcript::new();
        let (_agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_err(), "the network death fails the chain");

        let saved = HostAgent::load_session_from(&save).unwrap();
        let snapshot = rhema_submissions::snapshot(&saved).unwrap();
        let record = snapshot.get(&id).expect("the chain's record survived");
        assert_eq!(record.status, rhema_submissions::Status::Failed);
        assert!(matches!(
            &record.outcome,
            Some(Outcome::Failed {
                retryable: true,
                ..
            })
        ));
    }

    /// A provider failing the way a rejected key does: permanent to
    /// the ladder's eye, re-armable to `/retry`'s hand.
    struct FailsPermanent;

    #[async_trait::async_trait]
    impl ModelProvider for FailsPermanent {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Err(ProviderError::Api {
                message: "bad key".into(),
                status: Some(401),
            })
        }
    }

    /// A 4xx failure settles re-armable too: `/retry` re-sends the
    /// failed request whatever the class (the rate-limit story is
    /// `/model` then `/retry`), so the ledger's gate must not refuse
    /// the re-arm the offer promises.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_permanently_failed_chain_settles_rearmable_and_saves() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let mut agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(FailsPermanent) as Arc<dyn ModelProvider>,
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let id = start_submission(&mut agent, "hi");
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            Some(id.clone()),
            cancelled,
            steered,
            tx,
        );
        let mut transcript = Transcript::new();
        let (_agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_err(), "the rejected key fails the chain");

        let mut saved = HostAgent::load_session_from(&save).unwrap();
        let snapshot = rhema_submissions::snapshot(&saved).unwrap();
        let record = snapshot.get(&id).expect("the chain's record survived");
        assert_eq!(record.status, rhema_submissions::Status::Failed);
        assert!(matches!(
            &record.outcome,
            Some(Outcome::Failed {
                retryable: true,
                ..
            })
        ));
        // The re-arm the verdict just promised is granted, so `/retry`
        // has a record to run under.
        rhema_submissions::retry(&mut saved, &id, 2).expect("the settled record re-arms");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_chain_settles_cancelled_and_saves() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let mut agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(HangingProvider) as Arc<dyn ModelProvider>,
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let id = start_submission(&mut agent, "hi");
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            Some(id.clone()),
            cancelled,
            steered,
            tx,
        );
        cancel.send(true).unwrap();
        loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::TurnCancelled { .. } => break,
                UiEvent::TurnFinished { .. } => panic!("a cancelled chain must not finish"),
                _ => {}
            }
        }

        let saved = HostAgent::load_session_from(&save).unwrap();
        let snapshot = rhema_submissions::snapshot(&saved).unwrap();
        assert_eq!(
            snapshot.get(&id).unwrap().status,
            rhema_submissions::Status::Cancelled
        );
    }

    /// The first call fails the way a dropped connection does; the
    /// second answers and keeps the request it saw, so the test can
    /// compare what the retry actually sent.
    struct FailsThenSucceeds {
        calls: std::sync::atomic::AtomicU32,
        requests: std::sync::Mutex<Vec<rhema_core::ModelRequest>>,
    }

    impl Default for FailsThenSucceeds {
        fn default() -> Self {
            Self {
                calls: std::sync::atomic::AtomicU32::new(0),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for FailsThenSucceeds {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            self.requests.lock().unwrap().push(req);
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(ProviderError::Network("connection reset".into()))
            } else {
                Ok(scripted_output("recovered"))
            }
        }
    }

    /// The network drops the first request; the retry sends the same
    /// one: no new user message is pushed, the history grows by the
    /// answer alone, and the retried request carries the failed turn's
    /// prompt exactly as it went out the first time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_resends_the_failed_request_without_a_new_message() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, cancelled) = watch_cancel();
        let provider = Arc::new(FailsThenSucceeds::default());
        let agent = HostAgent::new(HostAgentConfig::new(
            Arc::clone(&provider) as Arc<dyn ModelProvider>,
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            None,
            None,
            cancelled,
            steered,
            tx,
        );

        let mut transcript = Transcript::new();
        let (agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert_eq!(
            result,
            Err("network error: connection reset".to_owned()),
            "the first turn reports the failure"
        );
        assert_eq!(agent.session().history.len(), 1, "the prompt committed");

        // /retry: the same request, no new message beside it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, cancelled) = watch_cancel();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(*agent, Prompt::Retry, None, None, cancelled, steered, tx);
        let mut transcript = Transcript::new();
        let (agent, result) = drive_turn_for_test(&mut rx, &mut transcript).await;
        assert!(result.is_ok(), "the retried request answers");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // The retry added the answer only, the failed turn's prompt is
        // not in the history twice.
        let history = &agent.session().history;
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0].role, Role::User));
        assert!(matches!(history[1].role, Role::Assistant));
        // And what went out was the identical conversation: one user
        // message, "hi".
        let requests = provider.requests.lock().unwrap();
        let sent = requests.last().expect("the retry issued a request");
        assert_eq!(sent.messages.len(), 1);
        assert_eq!(
            sent.messages[0]
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            "hi"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_streams_into_the_transcript_and_returns_the_agent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output(
                "hello agent",
            )])),
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            None,
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(rhema_core::MockProvider::new([scripted_output("done")])),
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            None,
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(HangingProvider),
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            None,
            None,
            cancelled,
            steered,
            tx,
        );

        cancel.send(true).unwrap();
        loop {
            match rx.recv().await.expect("turn task still alive") {
                UiEvent::TurnCancelled { agent, .. } => {
                    // At most the prompt committed, zero when the flag
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancelled) = watch_cancel();
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("session.json");
        let agent = HostAgent::new(HostAgentConfig::new(
            Arc::new(rhema_core::MockProvider::new(vec![sleeping_tool_output()])),
            "test-model",
            std::env::temp_dir(),
            rhema_host_agent::AllowAll,
        ))
        .unwrap();
        let (_steer_tx, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
        spawn_turn(
            agent,
            Prompt::text("hi"),
            Some(save.clone()),
            None,
            cancelled,
            steered,
            tx,
        );

        // The hook writes the intent before the tool hangs; wait until
        // the intent is on disk so the cancel cannot race ahead of the
        // commit. (The per-stream commits create the file earlier still
        //, the prompt alone, so existence alone proves nothing.)
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
                    // The file holds the committed intent (the user input
                    // and the assistant call, no result) or, when the
                    // cancel lost its race with the tool boundary, the
                    // closed round (the result is the cancellation).
                    let raw = std::fs::read_to_string(&save).unwrap();
                    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
                    let history = doc["history"].as_array().unwrap();
                    assert!(
                        matches!(history.len(), 2 | 3),
                        "the file ended at a tool boundary: {raw}"
                    );
                    let last = serde_json::to_string(history.last().expect("the file has history"))
                        .unwrap();
                    assert!(last.contains("call_1"), "intent committed: {last}");
                    if history.len() == 2 {
                        assert!(!last.contains("\"is_error\""), "no result yet: {last}");
                    } else {
                        assert!(
                            last.contains("interrupted"),
                            "the closed round is the cancellation: {last}"
                        );
                    }
                    return;
                }
                UiEvent::TurnFinished { agent, result, .. } => {
                    // The store's save is durable only after its
                    // directory fsync, and the file becomes visible at
                    // the rename before it. A cancel that lands in that
                    // window reaches the tool before it starts: the
                    // tool reports `cancelled` without running, the
                    // engine commits the closed round, and the next
                    // step finds the mock script exhausted. The side
                    // effect never ran, which is what the cancelled arm
                    // protects, one boundary later.
                    assert!(result.is_err(), "the exhausted provider fails the turn");
                    let last = agent
                        .session()
                        .history
                        .last()
                        .expect("the turn wrote history");
                    let closed = serde_json::to_string(last).unwrap();
                    assert!(
                        closed.contains("call_1") && closed.contains("cancelled"),
                        "the committed result is the cancellation: {closed}"
                    );
                    return;
                }
                _ => {}
            }
        }
    }
}
