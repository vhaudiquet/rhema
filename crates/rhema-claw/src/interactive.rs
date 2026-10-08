//! The interactive handler: one dispatch of `interactive:<name>` is one
//! chat turn, and the turn's final text is the reply the subscriber
//! sees.
//!
//! The open interactive surface never resolves keys (spec §6.2): every
//! message on `interactive:<name>` belongs to that session by
//! construction — the source's key function is the payload's `key`
//! verbatim, and the handler's only classification is "none". One
//! dispatch drains the inbox and folds every drained text into ONE user
//! input (the spec §3 batching rule), so a burst of quick messages costs
//! one reasoning step, not one per message.
//!
//! Continuity rides the dispatcher exactly as the forge handler's does:
//! the whole [`rhema_core::Session`] is stashed into
//! `ctx.session.core.driver_state` before the run returns and restored
//! from there on the next dispatch (`Null` means fresh). The session is
//! the record of the conversation; the event hub's stream is
//! best-effort.
//!
//! Deterministic setup first (spec §1, §8): the allowlist — permissive,
//! v1's starting point — and the resolver chain (the coding toolset over
//! the interactive workspace plus the watch tools) are assembled before
//! the first model call; the model never decides its own permissions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_autonomous_agent::{Allowlist, Handler, ResolverChain, SessionContext, async_trait};
use rhema_coding_agent::{AllowAll, CodingTools};
use rhema_core::{
    Agent, ChatParams, Content, Event as ModelEvent, EventSink, Message, Role, Session,
    ToolContext, run_turn,
};
use rhema_rsd::{Dispatcher, Event, Outcome, ResourceKey};
use serde_json::{Value, json};

use crate::hub::{EventHub, TurnEvent};
use crate::watch_store::ClawStore;

/// The prefix every interactive dispatch key carries:
/// `interactive:<name>` — the name is the caller's label for the
/// session (`web`, a chat window, a script).
pub const KEY_PREFIX: &str = "interactive:";

/// The source name the interactive messages ingest under.
pub const SOURCE: &str = "user";

/// How many tool rounds one chat turn may run before it is cut off —
/// the same shape the coding driver's limit takes; a follow-up message
/// keeps going.
const MAX_TURN_ITERATIONS: u32 = 16;

/// Registers the interactive source on its dispatcher: the passthrough
/// key function, the payload's `key` verbatim. A payload without a key
/// still gets its own unique key — the infallible-seam rule the forge
/// sources follow — so it can neither clobber a session nor wedge the
/// dispatcher. Run once, before the first ingest.
pub fn register_user(dispatcher: &Dispatcher) {
    dispatcher.register_source(
        SOURCE,
        Arc::new(move |event: &Event| {
            event
                .payload
                .get("key")
                .and_then(Value::as_str)
                .map(ResourceKey::from)
                .unwrap_or_else(|| {
                    ResourceKey::from(format!("{KEY_PREFIX}unroutable#{}", event.id))
                })
        }),
    );
}

/// The event one chat message becomes. The id is unique per message (a
/// wall-clock millisecond plus a process counter), so a redelivery
/// never dedups a different message away.
pub fn user_event(key: &str, text: &str) -> Event {
    static MESSAGE_SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Event {
        id: format!(
            "msg-{}-{:04x}",
            now.as_millis(),
            MESSAGE_SEQ.fetch_add(1, Ordering::Relaxed)
        ),
        source: SOURCE.to_owned(),
        payload: json!({ "key": key, "text": text }),
        received_at: now.as_millis() as u64,
    }
}

/// The resident agent: a provider, a model, one shared workspace, the
/// watch table its tools manage, and the hub its turns stream through.
pub struct InteractiveHandler {
    provider: Arc<dyn rhema_core::ModelProvider>,
    model: String,
    workspace: PathBuf,
    tools: Arc<CodingTools>,
    watch_resolver: Arc<dyn rhema_core::ToolResolver>,
    hub: Arc<EventHub>,
}

impl InteractiveHandler {
    /// A handler whose chat turns run in `workspace` (the interactive
    /// scratch space under the daemon's workspace root) and whose watch
    /// tools mutate `watches`. Approval is v1-permissive — the shell is
    /// the point of a resident coding agent — installed once, since
    /// resolution is synchronous within a turn.
    pub fn new(
        provider: Arc<dyn rhema_core::ModelProvider>,
        model: impl Into<String>,
        workspace: impl Into<PathBuf>,
        watches: Arc<ClawStore>,
        hub: Arc<EventHub>,
    ) -> Self {
        let workspace = workspace.into();
        let tools = Arc::new(CodingTools::new(&workspace));
        tools.install_approval(Arc::new(AllowAll));
        let watch_resolver =
            crate::watch_tools::registry(watches).resolver_for(&Allowlist::permissive());
        Self {
            provider,
            model: model.into(),
            workspace,
            tools,
            watch_resolver,
            hub,
        }
    }

    /// The agent for one dispatch: rebuilt per run over the restored
    /// session (the engine's own step counter is per-agent; the
    /// conversation lives in the session, not the agent).
    fn agent(&self) -> Result<Agent, String> {
        Agent::builder()
            .provider(self.provider.clone())
            .resolver(Arc::new(ResolverChain::new(vec![
                self.tools.clone(),
                self.watch_resolver.clone(),
            ])))
            .system(persona(&self.workspace))
            .params(ChatParams::new(self.model.clone()))
            .build()
            .map_err(|err| format!("cannot build the interactive agent: {err}"))
    }

    /// One dispatch: drain, restore, turn, stash, reply.
    async fn dispatch(&self, ctx: &mut SessionContext) -> Result<Option<String>, String> {
        let key = ctx.key.as_str().to_owned();
        // Renews the lease and drains: every text since the last poll,
        // folded into one input (spec §3's batching rule).
        let items = ctx
            .poll_inbox()
            .await
            .map_err(|err| format!("inbox poll for {key} failed: {err}"))?;
        let input = items
            .iter()
            .filter_map(|item| item.event.payload.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        if input.trim().is_empty() {
            return Err(format!("dispatch for {key} carried no text"));
        }

        let mut session = restore_session(&ctx.session.core.driver_state, &key)?;
        let mut agent = self.agent()?;
        let tools = ToolContext {
            cwd: self.workspace.clone(),
            run_id: key.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let mut sink = HubSink {
            key: ctx.key.clone(),
            hub: self.hub.clone(),
            tool_names: HashMap::new(),
        };
        let output = run_turn(
            &mut agent,
            &mut session,
            Message::text(Role::User, input),
            &mut sink,
            &tools,
            MAX_TURN_ITERATIONS,
        )
        .await
        .map_err(|err| format!("turn for {key} failed: {err}"))?;

        // Stash before anything else: the conversation survives even if
        // the subscriber never sees the stream.
        ctx.session.core.driver_state = serde_json::to_value(&session)
            .map_err(|err| format!("session for {key} does not serialize: {err}"))?;
        Ok(final_text(&output.message))
    }
}

#[async_trait]
impl Handler for InteractiveHandler {
    async fn handle(&self, ctx: &mut SessionContext) -> Outcome {
        let key = ctx.key.clone();
        match self.dispatch(ctx).await {
            Ok(reply) => {
                self.hub.emit(
                    &key,
                    TurnEvent::TurnDone {
                        reply,
                        outcome: "handled".to_owned(),
                    },
                );
                Outcome::Handled
            }
            Err(reason) => {
                // The end marker matters most when the run failed: a
                // stream that just goes quiet is indistinguishable from
                // a hang.
                self.hub.emit(
                    &key,
                    TurnEvent::TurnDone {
                        reply: None,
                        outcome: "failed".to_owned(),
                    },
                );
                Outcome::Failed(reason)
            }
        }
    }
}

/// The stashed conversation: a serialized [`Session`], `Null` meaning
/// fresh. A stash that does not parse is loud — silently resetting would
/// re-orient the agent mid-conversation without anyone knowing.
fn restore_session(stashed: &Value, key: &str) -> Result<Session, String> {
    match stashed {
        Value::Null => Ok(Session::default()),
        stashed => serde_json::from_value(stashed.clone())
            .map_err(|err| format!("stashed session for {key} does not parse: {err}")),
    }
}

/// The resident agent's persona: what it is, where its shell runs, and
/// what it can manage — tersely, because this enters every request.
fn persona(workspace: &Path) -> String {
    format!(
        "You are rhema-claw, the resident agent on this machine. You \
         answer chat here; your shell runs in the workspace at {} — \
         read code, run tests, make edits there. You manage your own \
         watch list with the watch_repo, unwatch_repo, and list_watches \
         tools: a watched repository's activity (issues, pull requests, \
         comments) arrives as its own dispatch. Keep replies terse and \
         concrete; do the work with tools instead of narrating it. Your \
         final message — the one you end the turn on, with no tool calls \
         after it — is the reply the user sees.",
        workspace.display()
    )
}

/// The text a turn ends on: the assistant message's text parts, joined
/// and trimmed. `None` when the model ended without writing a reply.
fn final_text(message: &Message) -> Option<String> {
    let joined = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text(text) => Some(text.as_str()),
            Content::ToolCall(_) | Content::ToolResult(_) => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// A tool result's bounded summary for the stream: the first line, cut
/// at 72 characters like the daemon's own log lines.
fn snippet(content: &str) -> String {
    let line = content.lines().next().unwrap_or("");
    let mut cut = line.chars().take(72).collect::<String>();
    if line.chars().count() > 72 {
        cut.push('…');
    }
    cut
}

/// Maps the engine's events onto the hub, live. Tool names ride the
/// `ToolCallStart` the engine guarantees before each result; a name the
/// sink somehow never heard falls back to the call id rather than an
/// unnamed event.
struct HubSink {
    key: ResourceKey,
    hub: Arc<EventHub>,
    tool_names: HashMap<String, String>,
}

impl EventSink for HubSink {
    fn emit(&mut self, event: ModelEvent<'_>) {
        let turn = match event {
            ModelEvent::ThinkingDelta(text) => TurnEvent::ThinkingDelta {
                text: text.to_owned(),
            },
            ModelEvent::TextDelta(text) => TurnEvent::TextDelta {
                text: text.to_owned(),
            },
            ModelEvent::ToolCallStart { id, name } => {
                self.tool_names.insert(id.to_owned(), name.to_owned());
                TurnEvent::ToolStart {
                    name: name.to_owned(),
                }
            }
            ModelEvent::ToolResult {
                id,
                content,
                is_error,
                ..
            } => TurnEvent::ToolResult {
                name: self
                    .tool_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_owned()),
                is_error,
                summary: snippet(content),
            },
            // Step bookkeeping is the engine's business, not the wire's.
            ModelEvent::StepStart { .. }
            | ModelEvent::ToolCallArgsDelta(_)
            | ModelEvent::StepEnd { .. } => return,
        };
        self.hub.emit(&self.key, turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::{
        Message as CoreMessage, MockProvider, ModelOutput, Role as CoreRole, StopReason, Usage,
    };
    use rhema_rsd::{
        DispatcherConfig, KeyState, Outcome as RsdOutcome, ResourceKey as Key, VolatileStore,
    };

    fn reply(text: &str) -> ModelOutput {
        ModelOutput {
            message: CoreMessage::text(CoreRole::Assistant, text),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    #[test]
    fn the_persona_names_the_workspace_and_the_tools() {
        let prompt = persona(Path::new("/state/claw/workspaces/interactive"));
        assert!(
            prompt.contains("/state/claw/workspaces/interactive"),
            "{prompt}"
        );
        for tool in ["watch_repo", "unwatch_repo", "list_watches"] {
            assert!(prompt.contains(tool), "missing {tool}: {prompt}");
        }
        assert!(prompt.contains("final message"), "{prompt}");
    }

    #[test]
    fn user_events_carry_the_key_and_unique_ids() {
        let first = user_event("interactive:web", "hello");
        let second = user_event("interactive:web", "again");
        assert_eq!(first.source, "user");
        assert_eq!(first.id, first.id, "stable within itself");
        assert_ne!(first.id, second.id, "messages never share a dedup id");
        assert_eq!(
            first.payload,
            json!({"key": "interactive:web", "text": "hello"})
        );
    }

    #[test]
    fn the_passthrough_key_fn_reads_the_payload_verbatim() {
        let dispatcher = Dispatcher::new(
            DispatcherConfig::default(),
            Arc::new(rhema_rsd::SystemClock),
            Arc::new(VolatileStore::new()),
        );
        register_user(&dispatcher);
        let event = user_event("interactive:web", "hello");
        // Ingesting resolves the key through the registered fn: the
        // record lands under exactly the payload's key.
        dispatcher.ingest(event).expect("ingests");
        let status = dispatcher
            .status(&Key::from("interactive:web"))
            .expect("status reads");
        assert_eq!(status.inbox_depth, 1);

        // A keyless payload still routes, to its own unparsable slot.
        dispatcher
            .ingest(Event {
                id: "lonely".into(),
                source: "user".into(),
                payload: json!({ "text": "no key" }),
                received_at: 1,
            })
            .expect("ingests");
        assert!(
            dispatcher
                .status(&Key::from("interactive:unroutable#lonely"))
                .expect("status reads")
                .inbox_depth
                >= 1
        );
    }

    #[test]
    fn the_final_text_joins_and_trims_but_not_tool_calls() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("on it".into()),
                Content::Text(" — done.".into()),
            ],
        };
        assert_eq!(final_text(&message).as_deref(), Some("on it — done."));
        let empty = Message {
            role: Role::Assistant,
            content: vec![Content::Text("   ".into())],
        };
        assert_eq!(final_text(&empty), None);
    }

    #[test]
    fn snippets_cut_the_first_line_at_72_characters() {
        assert_eq!(snippet("exit code 0\n--- stdout ---"), "exit code 0");
        let long = "x".repeat(100);
        let cut = snippet(&long);
        assert_eq!(cut.chars().count(), 73, "72 plus the ellipsis: {cut}");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_null_stash_is_fresh_and_an_unparsable_one_is_loud() {
        assert!(
            restore_session(&Value::Null, "interactive:web")
                .expect("null is fresh")
                .history
                .is_empty()
        );
        let err = restore_session(&json!({"history": "garbage"}), "interactive:web")
            .expect_err("garbage is loud");
        assert!(err.contains("does not parse"), "{err}");
    }

    /// The full dispatch, in-process: one message in, one scripted reply
    /// out, the turn streamed through the hub, the conversation stashed.
    #[tokio::test]
    async fn a_dispatch_drains_replies_and_stashes_the_conversation() {
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            Arc::new(MockProvider::new([reply("hi back")])),
            "test-model",
            workspace.path().join("interactive"),
            store,
            hub.clone(),
        );

        let dispatcher = Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(rhema_rsd::SystemClock),
            Arc::new(VolatileStore::new()),
        ));
        register_user(&dispatcher);

        let key = Key::from("interactive:web");
        let mut receiver = hub.subscribe(&key);
        dispatcher
            .ingest(user_event(key.as_str(), "hello"))
            .expect("ingests");

        let driver = rhema_autonomous_agent::Driver::new(dispatcher.clone(), Arc::new(handler))
            .with_holder("test");
        let outcomes = driver.run_once().await.expect("the run happens");
        assert_eq!(outcomes, vec![RsdOutcome::Handled]);

        // The stream saw the reply and the end marker.
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::TextDelta {
                text: "hi back".into()
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::TurnDone {
                reply: Some("hi back".into()),
                outcome: "handled".into()
            }
        );
        // The key is back to idle: the dispatch is done.
        assert_eq!(
            dispatcher.status(&key).expect("status reads").state,
            KeyState::Idle
        );
    }

    /// The stash proof, done properly through the dispatcher's record.
    #[tokio::test]
    async fn a_dispatch_stashes_the_exchange_in_driver_state() {
        use rhema_rsd::Store;

        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            Arc::new(MockProvider::new([reply("hi back")])),
            "test-model",
            workspace.path().join("interactive"),
            store,
            hub,
        );
        let backing = Arc::new(VolatileStore::new());
        let dispatcher = Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(rhema_rsd::SystemClock),
            backing.clone(),
        ));
        register_user(&dispatcher);
        let key = Key::from("interactive:web");
        dispatcher
            .ingest(user_event(key.as_str(), "hello"))
            .expect("ingests");
        let driver = rhema_autonomous_agent::Driver::new(dispatcher, Arc::new(handler));
        driver.run_once().await.expect("the run happens");

        let record = backing
            .load(&key)
            .expect("the record loads")
            .expect("the key has a record");
        let stashed: Session =
            serde_json::from_value(record.session.expect("a session").core.driver_state)
                .expect("the stash parses back into a session");
        let roles: Vec<(CoreRole, String)> = stashed
            .history
            .iter()
            .filter_map(|message| match &message.content[0] {
                Content::Text(text) => Some((message.role, text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            roles,
            vec![
                (CoreRole::User, "hello".to_owned()),
                (CoreRole::Assistant, "hi back".to_owned()),
            ]
        );
    }
}
