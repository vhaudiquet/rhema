//! The coding-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop over `rhema_core::run_turn`.
//!
//! The agent owns one [`CodingTools`] resolver for its lifetime. Approval
//! arrives per turn; it is installed into the resolver before stepping and
//! cleared after (see [`crate::approval`] for why the hook travels as an
//! `Arc`). The session is plain data — the driver may mutate it between
//! turns, checkpoint it, or resume it from disk. A turn entering with
//! committed-but-unanswered calls (a crash resume) revives first:
//! replay-safe tools re-run, the rest are reported interrupted — see
//! [`revive_dangling_calls`].

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rhema_core::{
    Agent, AgentError, ChatParams, CommitHook, Content, EventSink, Message, ModelOutput,
    ModelProvider, ModelRequest, ProviderError, ReplaySafety, Role, Session, ToolCall, ToolContext,
    ToolResolver, ToolResult, async_trait, run_turn,
};

use crate::approval::Approval;
use crate::config::CodingAgentConfig;
use crate::tools::CodingTools;

/// Why a turn or a session operation failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodingAgentError {
    /// The engine or the provider failed, including a turn that exceeded
    /// the step budget (`AgentError::MaxIterations`).
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// A path resolved outside the workspace root. Inside a turn this
    /// surfaces as an in-band `is_error` tool result instead (SPEC.md §4);
    /// the variant exists for callers validating paths outside the tool
    /// loop.
    #[error("workspace escape: {0}")]
    WorkspaceEscape(std::path::PathBuf),
    /// The system-prompt template failed to render — bad syntax, unknown
    /// filter — with the template position when known. The agent refuses
    /// to start on a prompt it cannot fully render.
    #[error("system prompt template: {0}")]
    Template(String),
}

impl From<minijinja::Error> for CodingAgentError {
    fn from(err: minijinja::Error) -> Self {
        let line = err
            .line()
            .map(|line| format!(", line {line}"))
            .unwrap_or_default();
        Self::Template(format!("{err}{line}"))
    }
}

/// Expands embedded pastes in user text before a request leaves
/// (`rhema_core::paste`): the session carries the compact chip form a
/// UI renders, the model sees only the plain text. Wrapped around
/// every provider the agent is built with, so fresh and resumed
/// histories expand alike.
struct ExpandPastes {
    inner: Arc<dyn ModelProvider>,
}

#[async_trait]
impl ModelProvider for ExpandPastes {
    async fn complete(
        &self,
        mut req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        for message in &mut req.messages {
            if message.role != Role::User {
                continue;
            }
            for content in &mut message.content {
                if let Content::Text(text) = content {
                    *text = rhema_core::paste::expand(text);
                }
            }
        }
        self.inner.complete(req, sink).await
    }
}

/// A coding agent: drives [`run_turn`] over the built-in tools, one turn at
/// a time, keeping the history in a caller-visible [`Session`].
pub struct CodingAgent {
    agent: Agent,
    config: CodingAgentConfig,
    session: Session,
    tools: Arc<CodingTools>,
    turns: u64,
}

impl CodingAgent {
    /// A fresh agent with an empty session. The workspace is canonicalized
    /// for tool scoping; the system prompt defaults to the built-in
    /// template rendered by [`crate::prompt::render_system`] unless
    /// `config.system` replaces it. A base prompt that fails to render
    /// fails the construction.
    pub fn new(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
    ) -> Result<Self, CodingAgentError> {
        // The paste expansion rides the provider seam, not the engine:
        // the session keeps the compact form, and this one wrapper is
        // every request path's way out.
        let provider: Arc<dyn rhema_core::ModelProvider> =
            Arc::new(ExpandPastes { inner: provider });
        let tools = Arc::new(CodingTools::new(config.workspace.clone()));
        let system = match config.system.clone() {
            // An explicit total replacement is the escape hatch:
            // verbatim, not templated.
            Some(system) => system,
            None => {
                // An explicit base from the user's SYSTEM.md replaces
                // the built-in default; a base with no skills or tools
                // section shows none. The global skills directory may
                // not exist (no HOME) — as normal as an empty one.
                let global = rhema_config::Config::skills_dir().ok();
                let skills = crate::skills::discover(global.as_deref(), tools.workspace());
                let base = config
                    .base_system
                    .as_deref()
                    .unwrap_or(crate::prompt::DEFAULT_SYSTEM_MD);
                crate::prompt::render_system(base, tools.workspace(), &skills, &tools.specs())?
            }
        };
        let agent = Agent::builder()
            .provider(provider)
            .resolver(tools.clone())
            .system(system)
            .params(ChatParams {
                thinking: config.thinking,
                ..ChatParams::new(config.model.clone())
            })
            .build()?;
        Ok(Self {
            agent,
            config,
            session: Session::default(),
            tools,
            turns: 0,
        })
    }

    /// A fresh agent resuming `session` (e.g. from
    /// [`CodingAgent::load_session_from`]); the next turn continues the
    /// prior history.
    pub fn from_session(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
        session: Session,
    ) -> Result<Self, CodingAgentError> {
        let mut agent = Self::new(provider, config)?;
        agent.session = session;
        Ok(agent)
    }

    /// Installs the commit-before-execute hook on the engine (see
    /// [`rhema_core::CommitHook`]); `None` disables it. A driver that
    /// persists the session sets this so a crash inside a tool call leaves
    /// a committed intent — which the next turn's revive closes — rather
    /// than no record at all.
    pub fn set_commit_hook(&mut self, hook: Option<CommitHook>) {
        self.agent.set_commit_hook(hook);
    }

    /// The conversation so far.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The rendered system prompt this agent stamps into every request.
    pub fn system(&self) -> &str {
        self.agent.system()
    }

    /// Mutable access between turns; the engine owns the history only
    /// inside a turn.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// One turn: push the user input, loop steps until the model stops
    /// requesting tools (capped at `max_turn_steps`), with `approval`
    /// gating the side-effecting tools. Appends to the session in place;
    /// the hook is cleared on the way out, success or failure. A history
    /// entering with unanswered calls — a crash resume — revives first, so
    /// the model sees a closed round, not a dangling one.
    pub async fn turn(
        &mut self,
        input: impl Into<String>,
        sink: &mut dyn EventSink,
        approval: Arc<dyn Approval>,
    ) -> Result<ModelOutput, CodingAgentError> {
        self.turns += 1;
        let ctx = ToolContext {
            cwd: self.tools.workspace().to_path_buf(),
            run_id: format!("turn-{}", self.turns),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        revive_dangling_calls(&mut self.session, self.tools.as_ref(), &ctx).await;
        self.tools.install_approval(approval);
        let result = run_turn(
            &mut self.agent,
            &mut self.session,
            Message::text(Role::User, input),
            sink,
            &ctx,
            self.config.max_turn_steps,
        )
        .await;
        self.tools.clear_approval();
        Ok(result?)
    }

    /// Writes the session as pretty JSON to `path`.
    pub fn save_session_to(&self, path: &impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.session)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(path, json)
    }

    /// Reads a session written by [`CodingAgent::save_session_to`].
    pub fn load_session_from(path: &impl AsRef<Path>) -> io::Result<Session> {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

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

    #[async_trait]
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

#[cfg(test)]
mod paste_tests {
    use super::*;
    use crate::approval::AllowAll;
    use rhema_core::{Event, StopReason, Usage};
    use std::sync::Mutex;

    /// Records the user text each request carried, then ends the turn.
    struct Recording(Mutex<Vec<String>>);

    #[async_trait]
    impl ModelProvider for Recording {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let mut user = String::new();
            for message in &req.messages {
                if message.role != Role::User {
                    continue;
                }
                for content in &message.content {
                    if let Content::Text(text) = content {
                        user.push_str(text);
                    }
                }
            }
            self.0.lock().unwrap().push(user);
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    #[tokio::test]
    async fn the_provider_sees_pastes_expanded_and_the_session_keeps_the_chips() {
        let payload = "l1\nl2\nl3\nl4";
        let embedded = rhema_core::paste::embed(
            &format!("look at {} this", rhema_core::paste::MARK),
            &[payload.to_owned()],
        );
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let mut agent = CodingAgent::new(
            recording.clone(),
            CodingAgentConfig::new("m", std::env::temp_dir()),
        )
        .unwrap();
        agent
            .turn(&embedded, &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

        // The session records the compact chip form.
        assert_eq!(
            agent.session().history[0],
            Message::text(Role::User, embedded.clone())
        );
        // The provider saw the plain expansion, never the encoding.
        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], format!("look at {payload} this"));
    }
}
