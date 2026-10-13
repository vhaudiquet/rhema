//! The resident handler: one dispatch of a resident-context key is one
//! agent turn, and the turn's final text is the reply.
//!
//! **The prefix contract** — the one split between the two handlers on
//! the shared store, documented here and nowhere else: every key the
//! resident dispatcher's handler answers to starts with one of
//! [`RESIDENT_PREFIXES`] — `interactive:<name>` for the live chat
//! surface, `task:<n>` for scheduled tasks, `phone:<device>` for the
//! phone client's sessions. Forge keys (they carry `#`) belong to the
//! forge handler. [`is_resident_key`] is the contract's only arbiter;
//! the HTTP surface and the source key functions defer to it.
//!
//! The live interactive surface never resolves keys (spec §6.2): every
//! message on `interactive:<name>` belongs to that session by
//! construction — the source's key function is the payload's `key`
//! verbatim, and the handler's only classification is "none". One
//! dispatch drains the inbox and folds every drained text into ONE user
//! input (the spec §3 batching rule), so a burst of quick messages costs
//! one reasoning step, not one per message.
//!
//! **The delivery rule:** `interactive:` keys reply into the live chat
//! surface (the event hub's stream). Task and phone keys have no chat
//! surface to reply into, so the turn's final text auto-notifies the
//! key's device — `phone` unless the key names one — through the
//! notification table ([`crate::notifications`]); the `notify` tool
//! remains the model's explicit channel, this is the implicit one.
//!
//! Continuity rides the dispatcher exactly as the forge handler's does:
//! the whole [`rhema_core::Session`] is stashed into
//! `ctx.session.core.driver_state` before the run returns and restored
//! from there on the next dispatch (`Null` means fresh). The session is
//! the record of the conversation — the context the model returns to —
//! and the hub's record is the transcript: the stream is replayable,
//! the live deltas are the only best-effort part left. Commit-before-
//! execute rides it too: the run installs the context's commit hook, so
//! the intent is in the store before the first tool runs, and a
//! crash-left dangling call revives before the turn — safe tools re-run,
//! the rest report interrupted.
//!
//! Deterministic setup first (spec §1, §8): the allowlist — permissive,
//! v1's starting point — and the resolver chain (the coding toolset over
//! the interactive workspace, the memory tools, `bootstrap_save`, the
//! notify tool, the task tools, and the watch tools) are assembled
//! before the first model call; the model never decides its own
//! permissions. The system prompt is rendered per dispatch from the
//! bootstrap template over the live SOUL/USER files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rhema_autonomous_agent::{Allowlist, Handler, ResolverChain, SessionContext, async_trait};
use rhema_coding_agent::{AllowAll, CodingTools, ExpandPastes, revive_dangling_calls};
use rhema_core::{
    Agent, ChatParams, Content, Event as ModelEvent, EventSink, Message, ModelProvider, Role,
    Session, ToolContext, ToolResolver, run_turn,
};
use rhema_memory::{MemoryResolver, MemoryStore, Scope};
use rhema_rsd::{Dispatcher, Event, KeyFn, Outcome, ResourceKey};
use serde_json::{Value, json};

use crate::backends::BackendRegistry;
use crate::bootstrap::{Bootstrap, DEFAULT_SYSTEM_MD, render_system};
use crate::hub::{EventHub, TurnEvent};
use crate::notifications::NotificationStore;
use crate::watch_store::ClawStore;

/// The key prefixes of the resident context (see the module docs):
/// `interactive:`, `task:`, and `phone:`. Forge keys — they carry `#` —
/// belong to the forge handler; these three and nothing else belong to
/// this one.
pub const RESIDENT_PREFIXES: [&str; 3] = ["interactive:", "task:", "phone:"];

/// The prefix of the live chat surface's keys: `interactive:<name>` —
/// the name is the caller's label for the session (`web`, a chat
/// window, a script). The OpenAI shim speaks this prefix only.
pub const KEY_PREFIX: &str = "interactive:";

/// The source name the interactive messages ingest under.
pub const SOURCE: &str = "user";

/// The title an auto-notified reply carries: the resident's name — the
/// reply itself is the body, the key it came from is the session.
const AUTO_NOTIFY_TITLE: &str = "claw";

/// Whether `key` belongs to the resident context ([`RESIDENT_PREFIXES`])
/// — the check the messages route and the generic ingest gate keys on.
pub fn is_resident_key(key: &str) -> bool {
    RESIDENT_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// The notification device a resident key replies to: the suffix a
/// `phone:` key names (`phone:desk` → `desk`),
/// [`DEFAULT_DEVICE`](crate::notifications::DEFAULT_DEVICE) otherwise —
/// `task:` keys always notify the phone.
fn notify_device(key: &str) -> &str {
    key.strip_prefix("phone:")
        .filter(|device| !device.is_empty())
        .unwrap_or(crate::notifications::DEFAULT_DEVICE)
}

/// The passthrough key function the resident context's sources share
/// (`user`, `tasks`, and every generic `/v1/ingest` source): the
/// payload's `key` verbatim — one key, one owned session (spec §6.2) —
/// with the infallible unique-key fallback the forge sources use, so a
/// keyless payload can neither clobber a session nor wedge the
/// dispatcher; it lands in the resident context's unroutable slot.
pub fn passthrough_key_fn() -> Arc<KeyFn> {
    Arc::new(|event: &Event| {
        event
            .payload
            .get("key")
            .and_then(Value::as_str)
            .map(ResourceKey::from)
            .unwrap_or_else(|| ResourceKey::from(format!("{KEY_PREFIX}unroutable#{}", event.id)))
    })
}

/// Registers the interactive source on its dispatcher. Run once, before
/// the first ingest.
pub fn register_user(dispatcher: &Dispatcher) {
    dispatcher.register_source(SOURCE, passthrough_key_fn());
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
/// bootstrap store its identity files live in, the watch table its
/// tools manage, the notification table it reaches devices through, and
/// the hub its turns stream through.
pub struct InteractiveHandler {
    /// The deployment's providers and the active pair, read per dispatch
    /// so a runtime switch is live on the next turn.
    backends: Arc<BackendRegistry>,
    workspace: PathBuf,
    tools: Arc<CodingTools>,
    watch_resolver: Arc<dyn rhema_core::ToolResolver>,
    /// The identity tools: interactive-only by construction — the forge
    /// handler never resolves them, so a forge comment cannot reach the
    /// accounts.
    accounts_resolver: Arc<dyn rhema_core::ToolResolver>,
    notify_resolver: Arc<dyn rhema_core::ToolResolver>,
    task_resolver: Arc<dyn rhema_core::ToolResolver>,
    /// The durable cross-session store the memory tools act on; the
    /// resolver is built per dispatch so its scope tracks the workspace.
    memory: Arc<dyn MemoryStore>,
    /// The identity files: the system prompt renders from them per
    /// dispatch, and the `bootstrap_save` resolver writes them.
    bootstrap: Arc<Bootstrap>,
    bootstrap_resolver: Arc<dyn rhema_core::ToolResolver>,
    hub: Arc<EventHub>,
    /// The notification table: the `notify` tool writes it, and
    /// non-interactive replies auto-notify through it.
    notifications: Arc<NotificationStore>,
    /// The consent table the `ask_secret` tool asks through and the
    /// consent routes answer into.
    consents: Arc<crate::consents::ConsentStore>,
    /// The wake bridge between those routes and the tools waiting on
    /// their rows.
    bridge: Arc<crate::consents::WaitBridge>,
    /// The attachment store the `attach_file` tool copies into and the
    /// fetch route serves from.
    attachments: Arc<crate::attachments::AttachmentStore>,
    /// How many tool rounds one turn may run before it is cut off — the
    /// config's runaway guard for an agent meant to work unattended; a
    /// follow-up message keeps going.
    max_turn_steps: u32,
}

impl InteractiveHandler {
    /// A handler whose chat turns run in `workspace` (the interactive
    /// scratch space under the daemon's workspace root), whose watch
    /// tools mutate `watches`, whose notifications land in
    /// `notifications`, and whose task tools manage `tasks`. Approval is
    /// v1-permissive — the shell is the point of a resident coding
    /// agent — installed once, since resolution is synchronous within a
    /// turn.
    #[allow(clippy::too_many_arguments)] // thirteen parts, each load-bearing
    pub fn new(
        backends: Arc<BackendRegistry>,
        workspace: impl Into<PathBuf>,
        watches: Arc<ClawStore>,
        memory: Arc<dyn MemoryStore>,
        bootstrap: Arc<Bootstrap>,
        hub: Arc<EventHub>,
        notifications: Arc<NotificationStore>,
        tasks: Arc<crate::task_store::TaskStore>,
        accounts: Arc<dyn rhema_identity::IdentityStore>,
        max_turn_steps: u32,
        consents: Arc<crate::consents::ConsentStore>,
        bridge: Arc<crate::consents::WaitBridge>,
        attachments: Arc<crate::attachments::AttachmentStore>,
    ) -> Self {
        let workspace = workspace.into();
        let tools = Arc::new(CodingTools::new(&workspace));
        tools.install_approval(Arc::new(AllowAll));
        let watch_resolver =
            crate::watch_tools::registry(watches.clone()).resolver_for(&Allowlist::permissive());
        let accounts_resolver = crate::accounts_tools::registry(accounts, watches)
            .resolver_for(&Allowlist::permissive());
        let notify_resolver = crate::notify_tools::registry(notifications.clone())
            .resolver_for(&Allowlist::permissive());
        let task_resolver =
            crate::task_tools::registry(tasks).resolver_for(&Allowlist::permissive());
        let bootstrap_resolver =
            crate::bootstrap::registry(bootstrap.clone()).resolver_for(&Allowlist::permissive());
        Self {
            backends,
            workspace,
            tools,
            watch_resolver,
            accounts_resolver,
            notify_resolver,
            task_resolver,
            memory,
            bootstrap,
            bootstrap_resolver,
            hub,
            notifications,
            consents,
            bridge,
            attachments,
            max_turn_steps,
        }
    }

    /// The agent for one dispatch: rebuilt per run over the restored
    /// session (the engine's own step counter is per-agent; the
    /// conversation lives in the session, not the agent). The system
    /// prompt is re-rendered per dispatch, so a SOUL.md or USER.md the
    /// last turn wrote is live this turn, the onboarding section is
    /// gone the moment both files exist, and the tool roster is the
    /// dispatch chain's own specs — what the prompt lists is what the
    /// request advertises.
    fn agent(&self, key: &ResourceKey) -> Result<Agent, String> {
        let (provider, model) = self.backends.current();
        // Chat input may carry embedded pastes — the compact chip form
        // the UIs mint and the session keeps — so the provider wraps in
        // the same expansion the coding driver wraps its own with: the
        // model sees plain text, the transcript keeps the chips.
        let provider: Arc<dyn ModelProvider> = Arc::new(ExpandPastes::new(provider));
        let resolver = self.resolver(key);
        let system = render_system(
            DEFAULT_SYSTEM_MD,
            &self.workspace,
            &self.bootstrap,
            &resolver.specs(),
        )
        .map_err(|err| format!("cannot render the bootstrap system prompt: {err}"))?;
        Agent::builder()
            .provider(provider)
            .resolver(resolver)
            .system(system)
            .params(ChatParams::new(model))
            .build()
            .map_err(|err| format!("cannot build the interactive agent: {err}"))
    }

    /// The resolver chain one dispatch runs under: the coding toolset
    /// over the interactive workspace, then the resident toolsets. The
    /// consent and attachment tools build per dispatch — their asks and
    /// cards must land on the session that asked, and the handler
    /// serves every session.
    fn resolver(&self, key: &ResourceKey) -> Arc<ResolverChain> {
        Arc::new(ResolverChain::new(vec![
            self.tools.clone(),
            Arc::new(MemoryResolver::new(
                self.memory.clone(),
                Scope::Workspace(self.workspace.clone()),
            )),
            self.bootstrap_resolver.clone(),
            self.notify_resolver.clone(),
            self.task_resolver.clone(),
            self.watch_resolver.clone(),
            self.accounts_resolver.clone(),
            crate::consents::registry(
                self.consents.clone(),
                self.bridge.clone(),
                self.hub.clone(),
                key.clone(),
            )
            .resolver_for(&Allowlist::permissive()),
            crate::attachments::registry(
                self.attachments.clone(),
                self.workspace.clone(),
                self.hub.clone(),
                key.clone(),
            )
            .resolver_for(&Allowlist::permissive()),
        ]))
    }

    /// One dispatch: drain, restore, turn, stash, reply.
    async fn dispatch(&self, ctx: &mut SessionContext) -> Result<Option<String>, String> {
        let key = ctx.key.as_str().to_owned();
        // Renews the lease and drains: every text since the last poll,
        // folded into one input (spec §3's batching rule). Chat events
        // carry `text`, scheduled tasks carry `prompt` — both are turn
        // input, and a mixed batch folds in arrival order.
        let items = ctx
            .poll_inbox()
            .await
            .map_err(|err| format!("inbox poll for {key} failed: {err}"))?;
        let input = items
            .iter()
            .filter_map(|item| {
                item.event
                    .payload
                    .get("text")
                    .or_else(|| item.event.payload.get("prompt"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if input.trim().is_empty() {
            return Err(format!("dispatch for {key} carried no text"));
        }
        // The transcript's question, recorded before anything else can
        // stream: a replay shows what was asked, not only what was
        // answered. Raw input — the notification-contract framing below
        // is the model's business, not the viewer's.
        self.hub.emit(
            &ctx.key,
            TurnEvent::UserMessage {
                text: input.clone(),
            },
        );
        // A non-chat resident key has no subscriber to read the reply:
        // say so in the input, so the model writes for the notification
        // the final message becomes.
        let input = if is_resident_key(&key) && !key.starts_with(KEY_PREFIX) {
            format!(
                "[Turn on {key}: this session has no live chat surface — \
                 your final message is delivered to the user as a \
                 notification.]\n\n{input}"
            )
        } else {
            input
        };

        let mut session = restore_session(&ctx.session.core.driver_state, &key)?;
        let resolver = self.resolver(&ctx.key);
        let mut agent = self.agent(&ctx.key)?;
        // Commit-before-execute: the intent is in the store before the
        // first tool runs — and if the lease cannot be committed under
        // (expired, lost), the turn fails before any side effect. A
        // crash-left dangling call revives first: safe tools re-run, the
        // rest report interrupted.
        agent.set_commit_hook(Some(ctx.commit_hook()));
        let tools = ToolContext {
            cwd: self.workspace.clone(),
            run_id: key.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        revive_dangling_calls(&mut session, resolver.as_ref(), &tools).await;
        let mut sink = HubSink {
            key: ctx.key.clone(),
            hub: self.hub.clone(),
            tool_names: HashMap::new(),
            tool_args: HashMap::new(),
            streaming: None,
        };
        let output = run_turn(
            &mut agent,
            &mut session,
            Message::text(Role::User, input),
            &mut sink,
            &tools,
            self.max_turn_steps,
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
                self.auto_notify(&key, reply.as_deref());
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

impl InteractiveHandler {
    /// The auto-notify contract (see the module docs): a non-chat
    /// resident key's reply has no subscriber, so the final text posts
    /// as a notification on the key's device — `phone` unless the key
    /// names one. A failed push is logged, never failed over: the turn
    /// ran, and a re-dispatch would find an empty inbox. Interactive
    /// keys and reply-less turns notify nothing.
    fn auto_notify(&self, key: &ResourceKey, reply: Option<&str>) {
        if key.as_str().starts_with(KEY_PREFIX) {
            return;
        }
        let Some(text) = reply else {
            return;
        };
        let device = notify_device(key.as_str());
        if let Err(err) = self.notifications.push(device, AUTO_NOTIFY_TITLE, text) {
            eprintln!("[claw] warning: auto-notify for {key} failed: {err}");
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
    /// The arguments streamed per call id. A shell result's command line
    /// is only known once the call's args have fully streamed, and a
    /// result event carries none of them — so the sink keeps what the
    /// deltas delivered and parses it when the result lands.
    tool_args: HashMap<String, String>,
    /// The call whose args the deltas are currently streaming. The delta
    /// event carries no id of its own; it belongs to the call that last
    /// started.
    streaming: Option<String>,
}

/// The shell command a `bash` call ran, parsed out of the streamed
/// arguments JSON. A call whose args never streamed, or that parses to
/// something without a string `command`, renders without one — the TUI's
/// transcript makes the same bargain with the same deltas.
fn shell_command(args: Option<&str>) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(args?).ok()?;
    parsed.get("command")?.as_str().map(str::to_owned)
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
                self.tool_args.insert(id.to_owned(), String::new());
                self.streaming = Some(id.to_owned());
                TurnEvent::ToolStart {
                    name: name.to_owned(),
                }
            }
            ModelEvent::ToolCallArgsDelta(fragment) => {
                if let Some(id) = &self.streaming {
                    self.tool_args
                        .entry(id.clone())
                        .or_default()
                        .push_str(fragment);
                }
                return;
            }
            ModelEvent::ToolResult {
                id,
                content,
                is_error,
                code,
                ..
            } => {
                let name = self
                    .tool_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_owned());
                let command = matches!(name.as_str(), "bash" | "run_command")
                    .then(|| shell_command(self.tool_args.get(id).map(String::as_str)))
                    .flatten();
                TurnEvent::ToolResult {
                    name,
                    is_error,
                    summary: snippet(content),
                    code,
                    command,
                }
            }
            // Step bookkeeping is the engine's business, not the wire's;
            // a running tool's live output lands with its result.
            ModelEvent::StepStart { .. }
            | ModelEvent::ToolOutputDelta { .. }
            | ModelEvent::StepEnd { .. } => return,
        };
        self.hub.emit(&self.key, turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::notifications::NotificationStore;
    use crate::task_store::TaskStore;

    use crate::backends::{Backend, BackendRegistry};
    use rhema_core::{
        Content, Message as CoreMessage, MockProvider, ModelOutput, Role as CoreRole, StopReason,
        ToolCall, ToolResolver, Usage,
    };
    use rhema_rsd::{
        DispatcherConfig, KeyState, Outcome as RsdOutcome, ResourceKey as Key, VolatileStore,
    };

    /// A throwaway notification table, the shape the dispatch tests hand
    /// the handler.
    fn notifications() -> Arc<NotificationStore> {
        Arc::new(NotificationStore::in_memory().expect("store opens"))
    }

    /// A throwaway task table, the shape the dispatch tests hand the
    /// handler.
    fn tasks() -> Arc<TaskStore> {
        Arc::new(TaskStore::in_memory().expect("store opens"))
    }

    /// A throwaway account store: the identity tools need something to
    /// stand in, and the dispatch tests never touch it.
    fn accounts() -> Arc<dyn rhema_identity::IdentityStore> {
        Arc::new(rhema_identity::VolatileIdentityStore::new())
    }

    /// A throwaway consent table, the shape the dispatch tests hand the
    /// handler.
    fn consents() -> Arc<crate::consents::ConsentStore> {
        Arc::new(crate::consents::ConsentStore::in_memory().expect("store opens"))
    }

    /// A fresh wake bridge, the shape the dispatch tests hand the
    /// handler.
    fn bridge() -> Arc<crate::consents::WaitBridge> {
        Arc::new(crate::consents::WaitBridge::new())
    }

    /// A throwaway attachment store under one shared temp root: the
    /// dispatch tests never read it back, and the store must outlive
    /// the handlers it is handed to.
    fn attachments() -> Arc<crate::attachments::AttachmentStore> {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let root = ROOT.get_or_init(|| {
            let dir = std::env::temp_dir().join("rhema-claw-test-attachments");
            let _ = std::fs::remove_dir_all(&dir);
            dir
        });
        Arc::new(crate::attachments::AttachmentStore::open(root).expect("store opens"))
    }

    fn test_registry(provider: Arc<dyn rhema_core::ModelProvider>) -> Arc<BackendRegistry> {
        Arc::new(
            BackendRegistry::new(vec![Backend::headless("mock", provider)], None)
                .expect("a single mock backend always constructs"),
        )
    }

    fn reply(text: &str) -> ModelOutput {
        ModelOutput {
            message: CoreMessage::text(CoreRole::Assistant, text),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    /// A provider that keeps every request it is handed, so a test can
    /// assert on what the model would have seen.
    struct RecordingProvider {
        requests: std::sync::Mutex<Vec<rhema_core::ModelRequest>>,
    }

    impl RecordingProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn last_user_text(&self) -> String {
            let requests = self.requests.lock().unwrap();
            let last = requests.last().expect("a request was made");
            last.messages
                .iter()
                .rev()
                .find(|message| message.role == CoreRole::User)
                .and_then(|message| match &message.content[0] {
                    Content::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .expect("a user text")
        }
    }

    #[async_trait]
    impl rhema_core::ModelProvider for RecordingProvider {
        async fn complete(
            &self,
            request: rhema_core::ModelRequest,
            _sink: &mut dyn rhema_core::EventSink,
        ) -> Result<ModelOutput, rhema_core::ProviderError> {
            self.requests.lock().unwrap().push(request);
            Ok(reply("noted."))
        }
    }

    /// The paste contract across the provider seam: a chat input that
    /// carries an embedded paste (the compact form the UIs mint) is
    /// expanded before the request leaves, while the transcript the hub
    /// streams keeps the compact form.
    #[tokio::test]
    async fn an_embedded_paste_expands_for_the_model_and_not_for_the_transcript() {
        let recorder = RecordingProvider::new();
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            test_registry(recorder.clone()),
            workspace.path().join("interactive"),
            store,
            Arc::new(rhema_memory::VolatileStore::new()),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            hub.clone(),
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
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
        let driver = rhema_autonomous_agent::Driver::new(dispatcher.clone(), Arc::new(handler))
            .with_holder("test");
        let key = Key::from("interactive:web");
        dispatcher
            .ingest(user_event(
                key.as_str(),
                "review this:\u{FFFC}fn main() {}\nfn other() {}\u{FFFC}",
            ))
            .expect("ingests");
        driver.run_once().await.expect("the run happens");

        assert_eq!(
            recorder.last_user_text(),
            "review this:fn main() {}\nfn other() {}",
            "the model saw the payload, not the marks"
        );
        let (history, _) = hub.subscribe_with_history(&key);
        let saw_input = history.into_iter().find_map(|event| match event {
            TurnEvent::UserMessage { text } => Some(text),
            _ => None,
        });
        assert!(
            saw_input
                .expect("the transcript saw the question")
                .contains('\u{FFFC}'),
            "the transcript keeps the compact chip form"
        );
    }

    /// A scripted tool-call turn: the model asking for `name` with `args`.
    fn scripted_call(id: &str, name: &str, args: serde_json::Value) -> ModelOutput {
        ModelOutput {
            message: CoreMessage {
                role: CoreRole::Assistant,
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

    #[test]
    fn the_prompt_renders_the_dispatch_chain_and_the_workspace() {
        // The real chain, the shape a dispatch runs under: the prompt
        // must name every tool the chain advertises, with its schema —
        // what the prompt lists is what the request carries.
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            test_registry(Arc::new(MockProvider::new([reply("hi back")]))),
            workspace.path().join("interactive"),
            Arc::new(ClawStore::in_memory().expect("store opens")),
            Arc::new(rhema_memory::VolatileStore::new()),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            Arc::new(EventHub::default()),
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
        );
        let specs = handler.resolver(&Key::from("interactive:web")).specs();
        assert!(
            specs.iter().any(|spec| spec.name == "bash"),
            "the coding toolset is in the chain: {specs:?}"
        );
        let prompt = render_system(
            DEFAULT_SYSTEM_MD,
            Path::new("/state/claw/workspaces/interactive"),
            &handler.bootstrap,
            &specs,
        )
        .expect("renders");
        assert!(
            prompt.contains("/state/claw/workspaces/interactive"),
            "{prompt}"
        );
        for spec in &specs {
            assert!(
                prompt.contains(&spec.name),
                "missing {}: {prompt}",
                spec.name
            );
            assert!(
                prompt.contains(&spec.description),
                "missing {}'s description: {prompt}",
                spec.name
            );
        }
        assert!(
            prompt.contains(r#""required":["command"]"#),
            "bash's schema renders: {prompt}"
        );
        assert!(prompt.contains("final message"), "{prompt}");
        // A fresh store is pending onboarding: the offered setup renders.
        assert!(prompt.contains("## Setup"), "{prompt}");
        assert!(prompt.contains("(pending onboarding)"), "{prompt}");
    }

    #[test]
    fn the_prefix_contract_splits_the_resident_context_from_the_forge() {
        for key in ["interactive:web", "task:5", "phone:ana", "phone:"] {
            assert!(is_resident_key(key), "{key} is resident");
        }
        for key in [
            "github:acme/widgets#7",
            "gitea:acme/barn#1",
            "interactive",
            "tasks",
            "task",
        ] {
            assert!(!is_resident_key(key), "{key} is not resident");
        }
    }

    #[test]
    fn a_phone_key_names_its_device_and_a_task_key_notifies_the_phone() {
        assert_eq!(notify_device("task:5"), "phone");
        assert_eq!(notify_device("phone:desk"), "desk");
        assert_eq!(notify_device("phone:"), "phone", "no suffix, the default");
        assert_eq!(notify_device("interactive:web"), "phone", "never reached");
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

    /// A bash result carries its command line and exit code on the wire:
    /// the sink parses the command out of the call's streamed arguments.
    #[test]
    fn a_bash_result_streams_its_command_and_exit_code() {
        let hub = Arc::new(EventHub::default());
        let key = Key::from("interactive:web");
        let mut receiver = hub.subscribe(&key);
        let mut sink = HubSink {
            key: key.clone(),
            hub: hub.clone(),
            tool_names: HashMap::new(),
            tool_args: HashMap::new(),
            streaming: None,
        };
        sink.emit(ModelEvent::ToolCallStart {
            id: "c1",
            name: "bash",
        });
        sink.emit(ModelEvent::ToolCallArgsDelta(r#"{"command": "ls""#));
        sink.emit(ModelEvent::ToolCallArgsDelta(r#", "quiet": true}"#));
        sink.emit(ModelEvent::ToolResult {
            id: "c1",
            content: "exit 0\n--- stdout ---",
            is_error: false,
            code: Some(0),
        });
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::ToolStart {
                name: "bash".into()
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::ToolResult {
                name: "bash".into(),
                is_error: false,
                summary: "exit 0".into(),
                code: Some(0),
                command: Some("ls".into()),
            }
        );
    }

    /// A tool whose args never streamed, or that is not a shell, reports
    /// no command — the summary still rides.
    #[test]
    fn a_result_without_streamed_args_or_a_shell_name_carries_no_command() {
        let hub = Arc::new(EventHub::default());
        let key = Key::from("interactive:web");
        let mut sink = HubSink {
            key: key.clone(),
            hub: hub.clone(),
            tool_names: HashMap::new(),
            tool_args: HashMap::new(),
            streaming: None,
        };
        sink.emit(ModelEvent::ToolCallStart {
            id: "c1",
            name: "memory_save",
        });
        sink.emit(ModelEvent::ToolResult {
            id: "c1",
            content: "saved",
            is_error: false,
            code: None,
        });
        let (history, _) = hub.subscribe_with_history(&key);
        assert_eq!(
            history.last(),
            Some(&TurnEvent::ToolResult {
                name: "memory_save".into(),
                is_error: false,
                summary: "saved".into(),
                code: None,
                command: None,
            })
        );
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
            test_registry(Arc::new(MockProvider::new([reply("hi back")]))),
            workspace.path().join("interactive"),
            store,
            Arc::new(rhema_memory::VolatileStore::new()),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            hub.clone(),
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
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

        // The stream saw the question, the reply, and the end marker.
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::UserMessage {
                text: "hello".into()
            }
        );
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

    /// Memory rides the resolver chain across dispatches: the model saves
    /// in one session turn, and the record is in the durable store for
    /// every session after — searchable by the model, never injected.
    #[tokio::test]
    async fn a_memory_save_in_one_dispatch_is_recalled_in_the_next() {
        use rhema_memory::{MEMORY_SAVE, MemoryQuery};

        let memory = Arc::new(rhema_memory::VolatileStore::new());
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let workspace_path = workspace.path().join("interactive");
        let handler = InteractiveHandler::new(
            test_registry(Arc::new(MockProvider::new([
                scripted_call(
                    "call-save",
                    MEMORY_SAVE,
                    json!({"body": "the release channel is #deploys", "tags": ["release"]}),
                ),
                reply("remembered."),
                scripted_call("call-search", "memory_search", json!({"query": "release"})),
                reply("the release channel is #deploys."),
            ]))),
            workspace_path.clone(),
            store,
            memory.clone(),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            hub,
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
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
        let driver = rhema_autonomous_agent::Driver::new(dispatcher.clone(), Arc::new(handler))
            .with_holder("test");
        let key = Key::from("interactive:web");

        // Dispatch 1: the model saves through the chain's memory tools.
        dispatcher
            .ingest(user_event(key.as_str(), "remember the channel"))
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("first run"),
            vec![RsdOutcome::Handled]
        );

        // The record is in the store, in the handler's workspace scope.
        let found = memory
            .search(MemoryQuery::new("release"))
            .expect("search reads");
        assert!(
            found.iter().any(|record| record.body.contains("#deploys")),
            "the saved record is durable: {found:?}"
        );

        // Dispatch 2: the model searches the same chain and answers from
        // memory — a fresh session turn, the same durable store.
        dispatcher
            .ingest(user_event(key.as_str(), "where do releases go?"))
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("second run"),
            vec![RsdOutcome::Handled]
        );
        assert_eq!(
            memory
                .search(MemoryQuery::new("#deploys"))
                .expect("search reads")
                .len(),
            1,
            "the search turn did not duplicate the record"
        );
    }

    /// The bootstrap tool rides the resolver chain across dispatches,
    /// exactly as the memory tools do: the model saves SOUL.md in one
    /// turn and USER.md in the next, the files land through the store,
    /// and the status the onboarding section renders from flips — gone
    /// the moment both files exist.
    #[tokio::test]
    async fn a_bootstrap_save_in_one_dispatch_flips_the_status_for_the_next() {
        use crate::bootstrap::BOOTSTRAP_SAVE;

        let dir = tempfile::tempdir().expect("tempdir");
        let bootstrap =
            Arc::new(Bootstrap::new(dir.path().join("bootstrap")).expect("store opens"));
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            test_registry(Arc::new(MockProvider::new([
                scripted_call(
                    "call-soul",
                    BOOTSTRAP_SAVE,
                    json!({"kind": "soul", "content": "I am claw: terse, warm."}),
                ),
                reply("noted."),
                scripted_call(
                    "call-user",
                    BOOTSTRAP_SAVE,
                    json!({"kind": "user", "content": "The user is Ana."}),
                ),
                reply("setup is done."),
            ]))),
            workspace.path().join("interactive"),
            store,
            Arc::new(rhema_memory::VolatileStore::new()),
            bootstrap.clone(),
            hub,
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
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
        let driver = rhema_autonomous_agent::Driver::new(dispatcher.clone(), Arc::new(handler))
            .with_holder("test");
        let key = Key::from("interactive:web");

        // Dispatch 1: the model writes SOUL.md through the chain. A key
        // with a session continues, so both calls ride one key.
        dispatcher
            .ingest(user_event(key.as_str(), "call me claw"))
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("first run"),
            vec![RsdOutcome::Handled]
        );
        assert_eq!(
            bootstrap
                .load(crate::bootstrap::BootstrapKind::Soul)
                .expect("reads"),
            Some("I am claw: terse, warm.".to_owned())
        );
        let status = bootstrap.status().expect("status reads");
        assert!(status.soul);
        assert!(!status.user);
        assert!(bootstrap.needs_onboarding(), "user is still unwritten");

        // Dispatch 2: USER.md, same chain — and the pending flag clears.
        dispatcher
            .ingest(user_event(key.as_str(), "and I am Ana"))
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("second run"),
            vec![RsdOutcome::Handled]
        );
        let status = bootstrap.status().expect("status reads");
        assert!(status.soul && status.user);
        assert!(!bootstrap.needs_onboarding(), "both files exist");
    }

    /// The stash proof, done properly through the dispatcher's record.
    #[tokio::test]
    async fn a_dispatch_stashes_the_exchange_in_driver_state() {
        use rhema_rsd::Store;

        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let handler = InteractiveHandler::new(
            test_registry(Arc::new(MockProvider::new([reply("hi back")]))),
            workspace.path().join("interactive"),
            store,
            Arc::new(rhema_memory::VolatileStore::new()),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            hub,
            notifications(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
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

    /// The auto-notify contract, end to end: a `task:` dispatch runs the
    /// prompt as a turn and its reply lands in the notification table on
    /// the phone; an `interactive:` dispatch replies into the hub and
    /// notifies nothing.
    #[tokio::test]
    async fn a_task_dispatch_runs_the_prompt_and_auto_notifies_the_phone() {
        use rhema_rsd::Store;

        let notifications = notifications();
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let hub = Arc::new(EventHub::default());
        let workspace = tempfile::tempdir().expect("tempdir");
        let backing = Arc::new(VolatileStore::new());
        let handler = Arc::new(InteractiveHandler::new(
            test_registry(Arc::new(MockProvider::new([
                reply("the bread is done."),
                reply("hi back"),
            ]))),
            workspace.path().join("interactive"),
            store,
            Arc::new(rhema_memory::VolatileStore::new()),
            Arc::new(Bootstrap::new(workspace.path().join("bootstrap")).expect("store opens")),
            hub,
            notifications.clone(),
            tasks(),
            accounts(),
            crate::config::DEFAULT_MAX_TURN_STEPS,
            consents(),
            bridge(),
            attachments(),
        ));

        let dispatcher = Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(rhema_rsd::SystemClock),
            backing.clone(),
        ));
        register_user(&dispatcher);
        // The source registration the daemon's assembly performs.
        let _scheduler = crate::task_source::TaskScheduler::new(
            &dispatcher,
            Arc::new(rhema_rsd::SystemClock),
            tasks(),
        );
        let driver =
            rhema_autonomous_agent::Driver::new(dispatcher.clone(), handler).with_holder("test");

        // The event shape TaskScheduler::detect delivers: the prompt is
        // the payload's `prompt`, the key is the task's.
        dispatcher
            .ingest(Event {
                id: "task-1@1500".into(),
                source: crate::task_source::TASKS_SOURCE.into(),
                payload: json!({ "key": "task:1", "prompt": "check the bread" }),
                received_at: 1_500,
            })
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("the run happens"),
            vec![RsdOutcome::Handled]
        );

        // The reply reached the phone: the key names no device, so the
        // auto-notify rule answers to the default.
        let pending = notifications.pending("phone", 0).expect("poll reads");
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].device, "phone");
        assert_eq!(pending[0].title, "claw");
        assert_eq!(pending[0].body, "the bread is done.");

        // The turn ran with the prompt — the stashed session carries it
        // verbatim, under the delivery-contract framing.
        let record = backing
            .load(&Key::from("task:1"))
            .expect("the record loads")
            .expect("the key has a record");
        let stashed: Session =
            serde_json::from_value(record.session.expect("a session").core.driver_state)
                .expect("the stash parses");
        let user_texts: Vec<&str> = stashed
            .history
            .iter()
            .filter_map(|message| match &message.content[0] {
                Content::Text(text) if message.role == Role::User => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(user_texts.len(), 1, "{user_texts:?}");
        assert!(user_texts[0].contains("check the bread"), "{user_texts:?}");
        assert!(
            user_texts[0].contains("notification"),
            "the delivery contract rode in: {user_texts:?}"
        );

        // An interactive reply is the chat surface's business: notified
        // nowhere.
        dispatcher
            .ingest(user_event("interactive:web", "hello"))
            .expect("ingests");
        assert_eq!(
            driver.run_once().await.expect("the run happens"),
            vec![RsdOutcome::Handled]
        );
        assert_eq!(
            notifications.pending("phone", 0).expect("poll reads").len(),
            1,
            "still just the task's notification"
        );
    }
}
