//! The coding-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop over `rhema_core::run_turn`.
//!
//! The agent owns one [`ShellTools`] resolver for its lifetime. Approval
//! arrives per turn; it is installed into the resolver before stepping and
//! cleared after (see [`rhema_agent_tools::approval`] for why the hook travels
//! as an `Arc`). The session is plain data — the driver may mutate it between
//! turns, checkpoint it, or resume it from disk. A turn entering with
//! committed-but-unanswered calls (a crash resume) revives first:
//! replay-safe tools re-run, the rest are reported interrupted — see
//! [`revive_dangling_calls`].

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rhema_agent_tools::{Approval, ExpandPastes, Overlay, ShellTools, revive_dangling_calls};
use rhema_core::{
    Agent, AgentError, ChatParams, CommitHook, EventSink, Message, ModelOutput, Role, Session,
    ToolContext, ToolResolver, ToolSpec, run_turn,
};

use crate::config::CodingAgentConfig;

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

/// The system prompt an agent stamps into every request, assembled
/// outside an agent: `base` — the user's SYSTEM.md — when given, the
/// built-in default otherwise, rendered over `workspace` as given, the
/// discovered skills, and the tool specs — the built-ins plus
/// `extra`, the extension resolver's specs, so the prompt and the
/// request's tools array cannot drift. The one assembly point:
/// [`CodingAgent::new`] builds every agent's prompt here, and a driver
/// that previews the prompt calls the same function, so what is
/// previewed is what turns send.
pub fn stamped_system(
    base: Option<&str>,
    workspace: &Path,
    extra: &[ToolSpec],
) -> Result<String, CodingAgentError> {
    // The global skills directory may not exist (no HOME) — as normal
    // as an empty one.
    let global = rhema_config::Config::skills_dir().ok();
    let skills = rhema_agent_tools::skills::discover(global.as_deref(), workspace);
    let mut tools = ShellTools::builtin_specs();
    tools.extend_from_slice(extra);
    rhema_agent_tools::render_system(
        base.unwrap_or(crate::prompt::DEFAULT_SYSTEM_MD),
        workspace,
        &skills,
        &tools,
    )
    .map_err(CodingAgentError::from)
}

/// A coding agent: drives [`run_turn`] over the built-in tools, one turn at
/// a time, keeping the history in a caller-visible [`Session`].
pub struct CodingAgent {
    agent: Agent,
    config: CodingAgentConfig,
    session: Session,
    tools: Arc<ShellTools>,
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
        let provider: Arc<dyn rhema_core::ModelProvider> = Arc::new(ExpandPastes::new(provider));
        let tools = config
            .tools
            .clone()
            .unwrap_or_else(|| Arc::new(ShellTools::new(config.workspace.clone())));
        let resolver: Arc<dyn ToolResolver> = match config.extra_resolver.clone() {
            Some(extra) => Arc::new(Overlay::new(tools.clone(), extra)),
            None => tools.clone(),
        };
        let system = match config.system.clone() {
            // An explicit total replacement is the escape hatch:
            // verbatim, not templated.
            Some(system) => system,
            None => stamped_system(
                config.base_system.as_deref(),
                tools.workspace(),
                &config
                    .extra_resolver
                    .as_ref()
                    .map(|resolver| resolver.specs())
                    .unwrap_or_default(),
            )?,
        };
        let agent = Agent::builder()
            .provider(provider)
            .resolver(resolver)
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

#[cfg(test)]
mod paste_tests {
    use super::*;
    use rhema_agent_tools::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, Usage, async_trait,
    };
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

#[cfg(test)]
mod extra_tools_tests {
    use super::*;
    use rhema_agent_tools::AllowAll;
    use rhema_core::{
        Content, Event, ModelProvider, ModelRequest, ProviderError, StopReason, ToolCall,
        ToolResult, Usage, async_trait,
    };
    use std::sync::Mutex;

    use serde_json::json;

    struct NoopSink;

    impl EventSink for NoopSink {
        fn emit(&mut self, _event: Event<'_>) {}
    }

    /// An extension tool: `echo` answers with its own `text` argument.
    struct Echo;

    #[async_trait]
    impl rhema_core::ToolResolver for Echo {
        fn specs(&self) -> Vec<rhema_core::ToolSpec> {
            vec![rhema_core::ToolSpec::new(
                "echo",
                "Repeats the text argument back. The extension's canary.",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            )]
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            let text = call.args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ToolResult::success(call.id, format!("echo: {text}"))
        }
    }

    /// Scripts one tool-use round (calling `echo`) followed by an
    /// end-turn; records the tool names every request advertised.
    struct Scripted(Mutex<Vec<Vec<String>>>);

    #[async_trait]
    impl ModelProvider for Scripted {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let round = self.0.lock().unwrap().len();
            self.0
                .lock()
                .unwrap()
                .push(req.tools.iter().map(|t| t.name.clone()).collect());
            if round == 0 {
                Ok(ModelOutput {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![Content::ToolCall(ToolCall {
                            id: "c1".into(),
                            name: "echo".into(),
                            args: json!({"text": "through the overlay"}),
                            ..ToolCall::default()
                        })],
                    },
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                })
            } else {
                Ok(ModelOutput {
                    message: Message::text(Role::Assistant, "final"),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                })
            }
        }
    }

    #[tokio::test]
    async fn the_extra_resolver_reaches_the_request_and_resolves() {
        let scripted = Arc::new(Scripted(Mutex::new(Vec::new())));
        let config = CodingAgentConfig {
            extra_resolver: Some(Arc::new(Echo)),
            ..CodingAgentConfig::new("m", std::env::temp_dir())
        };
        let mut agent = CodingAgent::new(scripted.clone(), config).unwrap();
        let output = agent
            .turn("use the tool", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();

        // The extension tool resolved in-band and its result reached the
        // model, which then ended the turn.
        let text: String = output
            .message
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "final");
        let rounds = scripted.0.lock().unwrap();
        assert_eq!(rounds.len(), 2);
        assert!(
            rounds[0].contains(&"echo".to_owned()) && rounds[0].contains(&"bash".to_owned()),
            "the tools array carries both: {:?}",
            rounds[0]
        );
        // The prompt names the extension tool too: prompt and tools
        // array cannot drift.
        assert!(agent.system().contains("echo"), "{}", agent.system());
        assert!(
            agent.system().contains("canary"),
            "the extension's description is in the prompt: {}",
            agent.system()
        );
    }

    #[tokio::test]
    async fn without_an_extra_resolver_only_the_built_ins_are_advertised() {
        let scripted = Arc::new(Scripted(Mutex::new(Vec::new())));
        let mut agent = CodingAgent::new(
            scripted.clone(),
            CodingAgentConfig::new("m", std::env::temp_dir()),
        )
        .unwrap();
        agent
            .turn("hi", &mut NoopSink, Arc::new(AllowAll))
            .await
            .unwrap();
        let rounds = scripted.0.lock().unwrap();
        assert_eq!(rounds[0], vec!["bash".to_owned()]);
        assert!(!agent.system().contains("canary"));
    }

    #[test]
    fn stamped_system_carries_the_extra_specs() {
        let workspace = std::env::temp_dir();
        let spec = rhema_core::ToolSpec::new(
            "echo",
            "Repeats text back. The extension's canary.",
            json!({"type": "object"}),
        );
        let with = stamped_system(None, &workspace, std::slice::from_ref(&spec)).unwrap();
        assert!(with.contains("echo") && with.contains("canary"), "{with}");
        let without = stamped_system(None, &workspace, &[]).unwrap();
        assert!(!without.contains("canary"), "{without}");
    }
}
