//! The per-session configuration, as the client set it and the
//! platform stored it.
//!
//! [`SessionConfig`] is the durable answer to "what is this
//! conversation": which model, which base prompt, how approvals gate,
//! where the workspace lives and what seeds it, which skills render,
//! how many steps a turn may take. The frontend persists it wherever
//! its platform keeps durable bytes and hands it to the
//! [`AgentAssembly`](crate::AgentAssembly); every field carries a
//! default so a never-configured session still assembles.
#![forbid(unsafe_code)]

use rhema_agent_tools::Skill;

/// The default model, when the session carries no config yet.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-5";

/// The sandbox workspace path.
pub const DEFAULT_WORKSPACE: &str = "/workspace";

fn default_workspace() -> String {
    DEFAULT_WORKSPACE.to_owned()
}

fn default_max_turn_steps() -> u32 {
    128
}

/// One skill seeded into the sandbox and rendered into the prompt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillSeed {
    /// The skill's folder name.
    pub name: String,
    /// The frontmatter description — the trigger.
    pub description: String,
    /// The whole `SKILL.md`, frontmatter included.
    pub body: String,
}

/// The per-session configuration, persisted by the frontend and set by
/// a client over its config route.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionConfig {
    /// Model identifier, passed through to the provider.
    pub model: String,
    /// The session's SYSTEM.md, stored verbatim; `None` renders the
    /// built-in default.
    #[serde(default)]
    pub base_system: Option<String>,
    /// The approval policy gating `bash`.
    #[serde(default)]
    pub approval: crate::assembly::ApprovalPolicy,
    /// The workspace path inside the sandbox.
    #[serde(default = "default_workspace")]
    pub workspace: String,
    /// A git URL to clone when the workspace is first (re)seeded.
    #[serde(default)]
    pub seed_git: Option<String>,
    /// Skills seeded into the sandbox and rendered into the prompt.
    #[serde(default)]
    pub skills: Vec<SkillSeed>,
    /// Cap on steps per turn.
    #[serde(default = "default_max_turn_steps")]
    pub max_turn_steps: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            base_system: None,
            approval: crate::assembly::ApprovalPolicy::default(),
            workspace: default_workspace(),
            seed_git: None,
            skills: Vec::new(),
            max_turn_steps: default_max_turn_steps(),
        }
    }
}

impl SessionConfig {
    /// The skill list the prompt renders, against the paths the files
    /// were seeded at — the model reads the bodies through the shell.
    pub fn skills(&self) -> Vec<Skill> {
        self.skills
            .iter()
            .map(|seed| Skill {
                name: seed.name.clone(),
                description: seed.description.clone(),
                path: format!("{}/.rhema/skills/{}/SKILL.md", self.workspace, seed.name).into(),
            })
            .collect()
    }
}
