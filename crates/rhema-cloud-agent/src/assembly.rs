//! Agent assembly without a filesystem.
//!
//! [`AgentAssembly`] carries every input the cloud agent needs that the
//! terminal agent reads off disk: the model id, the session's SYSTEM.md
//! from durable storage, the skills seeded into the sandbox, the
//! approval policy. [`AgentAssembly::assemble`] builds a
//! [`CodingAgent`](rhema_coding_agent::CodingAgent) whose shell runs in
//! the sandbox and whose construction touches no host path — the
//! workspace string is meaningful only inside the sandbox.
#![forbid(unsafe_code)]

use std::sync::Arc;

use rhema_agent_tools::{Approval, ShellExecutor, ShellTools, Skill};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig, CodingAgentError};
use rhema_core::ModelProvider;

/// The static, per-session approval policy. It answers alone when no
/// client is connected to consult; the WebSocket layer layers its
/// round-trip on top for the undecided calls. Serialized into the
/// session config the DO stores.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Every gated call is refused: the default, where the session runs
    /// unattended or its client is away.
    #[default]
    DenyAll,
    /// The named tools run without consulting anyone; every other call
    /// is refused. The built-in set has one side-effecting tool, so a
    /// useful allowlist is usually empty or names read-only extensions.
    AllowList(Vec<String>),
    /// Every gated call runs. The explicit, deliberate opt-in for
    /// unattended sessions.
    AllowAll,
    /// Gated calls go to the connected client as an approval request;
    /// no client, or no answer, denies. The timeout and the transport
    /// live in the [`crate::ApprovalChannel`], not here.
    AskClient,
}

impl ApprovalPolicy {
    /// The approval hook answering from this policy alone — the
    /// fail-safe the turn installs; interactive sessions gate ahead of
    /// it through [`crate::gate::ClientGate`].
    pub fn approval(&self) -> Arc<dyn Approval> {
        Arc::new(PolicyApproval::new(self.clone()))
    }
}

/// The static half of the approval gate: answers from the policy, never
/// from a conversation.
#[derive(Debug, Clone)]
pub struct PolicyApproval {
    policy: ApprovalPolicy,
}

impl PolicyApproval {
    /// A hook answering from `policy`.
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// The policy this hook answers from.
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }
}

impl Approval for PolicyApproval {
    fn approve(&self, call: &rhema_core::ToolCall) -> bool {
        match &self.policy {
            ApprovalPolicy::DenyAll => false,
            ApprovalPolicy::AllowList(names) => names.contains(&call.name),
            ApprovalPolicy::AllowAll => true,
            // The static answer for an interactive policy is a denial:
            // nobody to consult.
            ApprovalPolicy::AskClient => false,
        }
    }
}

/// Everything the cloud agent needs besides the provider and the
/// sandbox, all of it from durable storage or the request.
pub struct AgentAssembly {
    /// Model identifier, passed through to the provider's request.
    pub model: String,
    /// The session's SYSTEM.md as stored; `None` renders the built-in
    /// default template.
    pub base_system: Option<String>,
    /// Skills collected from durable storage and already seeded into
    /// the sandbox workspace — each `Skill::path` must resolve inside
    /// [`Self::workspace`], because the model reads the bodies through
    /// the shell.
    pub skills: Vec<Skill>,
    /// Cap on steps per turn; past it the turn fails with
    /// `AgentError::MaxIterations`.
    pub max_turn_steps: u32,
    /// How much reasoning the model should do.
    pub thinking: rhema_core::request::ThinkingEffort,
    /// The workspace path inside the sandbox (`/workspace`).
    pub workspace: String,
    /// The bounded request view; `Some(default)` is the coding agent's
    /// own default, `None` sends whole histories.
    pub transcript: Option<rhema_transcript::TranscriptSettings>,
    /// The approval policy gating the side-effecting tools.
    pub approval: ApprovalPolicy,
    /// The channel `AskClient` consults, when the session has one.
    pub channel: Option<Arc<dyn crate::gate::ApprovalChannel>>,
}

impl AgentAssembly {
    /// Builds the agent: its tools are the built-in `bash` running on
    /// `sandbox_executor` (usually a [`ShellSandbox`](crate::ShellSandbox)
    /// over the session's sandbox), its prompt rendered over the
    /// assembly's base and skills. Construction reads no filesystem.
    pub fn assemble(
        &self,
        provider: Arc<dyn ModelProvider>,
        sandbox_executor: Arc<dyn ShellExecutor>,
    ) -> Result<CodingAgent, CodingAgentError> {
        let (config, _shell) = self.config(sandbox_executor);
        CodingAgent::new(provider, config)
    }

    /// Builds the agent resuming `session` — the checkpoint a previous
    /// instance persisted before eviction.
    pub fn assemble_from_session(
        &self,
        provider: Arc<dyn ModelProvider>,
        sandbox_executor: Arc<dyn ShellExecutor>,
        session: rhema_core::Session,
    ) -> Result<CodingAgent, CodingAgentError> {
        let (config, _shell) = self.config(sandbox_executor);
        rhema_coding_agent::CodingAgent::from_session(provider, config, session)
    }

    /// The config the assembly builds, with its shell — factored so a
    /// resume and a fresh start assemble identically.
    fn config(
        &self,
        sandbox_executor: Arc<dyn ShellExecutor>,
    ) -> (CodingAgentConfig, Arc<ShellTools>) {
        let shell = Arc::new(ShellTools::with_executor(
            self.workspace.clone(),
            sandbox_executor,
        ));
        // The gate rides as the extension resolver: it shadows `bash`
        // with the shell's own spec, decides, and delegates. The shell
        // stays the tool of record — spec, caps, error shaping.
        let gate = Arc::new(crate::gate::ClientGate::new(
            shell.clone(),
            self.approval.clone(),
            self.channel.clone(),
        ));
        let config = CodingAgentConfig {
            model: self.model.clone(),
            system: None,
            base_system: self.base_system.clone(),
            workspace: self.workspace.clone().into(),
            skills: Some(self.skills.clone()),
            max_turn_steps: self.max_turn_steps,
            thinking: self.thinking,
            tools: Some(shell.clone()),
            extra_resolver: Some(gate),
            // The cloud agent's commands run in the remote sandbox; a
            // local env layer has nothing to ride on.
            shell_env: std::collections::BTreeMap::new(),
            transcript: self.transcript,
            // No summary worker yet: the cloud harness has one model
            // handle, and a cheap second one is a later decision.
            summary_worker: None,
            steering: None,
            // Commands run in the deployment's remote sandbox, so the
            // environment section claims no host facts — the notice
            // names where bash runs and defers to the sandbox itself.
            environment: Some(rhema_coding_agent::remote_notice(
                "cloud",
                &self.workspace,
                &self.model,
                concat!("rhema-cloud-agent ", env!("CARGO_PKG_VERSION")),
            )),
            // No filesystem to read a USER.md from; a per-session
            // preference file is a later SessionConfig decision.
            user_preferences: None,
        };
        (config, shell)
    }
}
