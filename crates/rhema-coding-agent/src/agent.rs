//! The coding-agent driver: a provider, the built-in workspace tools, and
//! the per-turn loop over `rhema_core::run_turn`.
//!
//! The agent owns one [`CodingTools`] resolver for its lifetime. Approval
//! arrives per turn; it is installed into the resolver before stepping and
//! cleared after (see [`crate::approval`] for why the hook travels as an
//! `Arc`). The session is plain data — the driver may mutate it between
//! turns, checkpoint it, or resume it from disk.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rhema_core::{
    Agent, AgentError, ChatParams, EventSink, Message, ModelOutput, Role, Session, ToolContext,
    run_turn,
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
}

/// The built-in system prompt, kept as data next to the code and read
/// in whole — the full prompt, visible and editable in one place. Two
/// placeholders render into it (see [`render_system`]): `{workspace}`
/// and `{skills}`.
pub const DEFAULT_SYSTEM_MD: &str = include_str!("../SYSTEM.md");

/// Renders a SYSTEM.md template: `{workspace}` becomes the workspace
/// path, `{skills}` the skills table
/// ([`prompt_section`](crate::skills::prompt_section) — empty when no
/// skill was found, so the model never sees machinery for an empty
/// set). A template without `{skills}` gets the table appended, so a
/// prompt written before the placeholder keeps working. Any other
/// `{...}` text stays as written: templates are prose, not programs.
pub fn render_system(template: &str, workspace: &Path, skills: &[crate::skills::Skill]) -> String {
    let section = crate::skills::prompt_section(skills);
    let rendered = template
        .replace("{workspace}", &workspace.display().to_string())
        .replace("{skills}", &section);
    let rendered = if template.contains("{skills}") {
        rendered
    } else {
        rendered + &section
    };
    format!("{}\n", rendered.trim_end())
}

/// The built-in prompt for a workspace with no skills: [`DEFAULT_SYSTEM_MD`]
/// through [`render_system`] with an empty table.
pub fn default_system(workspace: &Path) -> String {
    render_system(DEFAULT_SYSTEM_MD, workspace, &[])
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
    /// for tool scoping; the system prompt defaults to [`default_system`]
    /// unless `config.system` replaces it.
    pub fn new(
        provider: Arc<dyn rhema_core::ModelProvider>,
        config: CodingAgentConfig,
    ) -> Result<Self, AgentError> {
        let tools = Arc::new(CodingTools::new(config.workspace.clone()));
        let system = config.system.clone().unwrap_or_else(|| {
            // An explicit base from the user's SYSTEM.md replaces the
            // built-in default; placeholders render either way — the
            // skills table lands where `{skills}` sits, or at the end
            // when the base predates the placeholder.
            let base = config
                .base_system
                .clone()
                .unwrap_or_else(|| DEFAULT_SYSTEM_MD.to_owned());
            // The global skills directory may not exist (no HOME) — as
            // normal as an empty one.
            let global = rhema_config::Config::skills_dir().ok();
            let skills = crate::skills::discover(global.as_deref(), tools.workspace());
            render_system(&base, tools.workspace(), &skills)
        });
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
    ) -> Result<Self, AgentError> {
        let mut agent = Self::new(provider, config)?;
        agent.session = session;
        Ok(agent)
    }

    /// The conversation so far.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Mutable access between turns; the engine owns the history only
    /// inside a turn.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// One turn: push the user input, loop steps until the model stops
    /// requesting tools (capped at `max_turn_steps`), with `approval`
    /// gating the side-effecting tools. Appends to the session in place;
    /// the hook is cleared on the way out, success or failure.
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
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> crate::skills::Skill {
        crate::skills::Skill {
            name: name.into(),
            description: description.into(),
            path: format!("/cfg/skills/{name}/SKILL.md").into(),
        }
    }

    #[test]
    fn render_fills_the_workspace_and_leaves_other_braces_alone() {
        let out = render_system("work in {workspace}, keep {mystery}", Path::new("/ws"), &[]);
        assert_eq!(out, "work in /ws, keep {mystery}\n");
    }

    #[test]
    fn render_puts_the_skills_table_where_the_placeholder_sits() {
        let out = render_system(
            "a\n{skills}\nb {workspace}",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(
            out,
            "a\n\nSkills (name / trigger / file):\ns / d / /cfg/skills/s/SKILL.md\n\nb /ws\n"
        );
    }

    #[test]
    fn render_without_the_placeholder_appends_the_table() {
        let out = render_system(
            "base in {workspace}\n",
            Path::new("/ws"),
            &[skill("s", "d")],
        );
        assert_eq!(
            out,
            "base in /ws\n\nSkills (name / trigger / file):\ns / d / /cfg/skills/s/SKILL.md\n"
        );
    }

    #[test]
    fn render_with_no_skills_leaves_no_machinery_behind() {
        let out = render_system("a\n{skills}\nb", Path::new("/ws"), &[]);
        assert_eq!(out, "a\n\nb\n");
        assert!(!out.contains("Skills"), "{out}");
    }

    #[test]
    fn the_default_prompt_ends_with_one_newline_and_no_placeholder() {
        let out = default_system(Path::new("/ws"));
        assert!(
            out.starts_with("You are a coding agent working in the workspace /ws."),
            "{out}"
        );
        assert!(!out.contains('{'), "{out}");
        assert!(out.ends_with("workspace root.\n"), "{out}");
    }
}
