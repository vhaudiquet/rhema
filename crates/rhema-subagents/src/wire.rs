//! The explicit wiring: frontends call [`wire`], the driver never
//! discovers subagents on its own.
//!
//! [`wire`] discovers definitions and, when any exist, attaches a
//! [`SubagentTools`] to a `CodingAgentConfig` — the shared shell and the
//! extra resolver the driver merges through its `Overlay`. With nothing
//! discovered the config comes back untouched, so a workspace without
//! subagents runs exactly as before. [`extra_specs`] feeds the same
//! discovery to a prompt preview, so what is previewed is what turns
//! send.

use std::path::Path;
use std::sync::Arc;

use rhema_agent_tools::ShellTools;
use rhema_coding_agent::CodingAgentConfig;
use rhema_core::{ModelProvider, ToolSpec};

use crate::def::discover;
use crate::tools::SubagentTools;

/// Wires discovered subagents into `config`, reading the global
/// definitions directory off the config dir. See [`wire_at`].
pub fn wire(provider: Arc<dyn ModelProvider>, mut config: CodingAgentConfig) -> CodingAgentConfig {
    let global = rhema_config::Config::dir().ok();
    wire_at(global.as_deref(), provider, &mut config);
    config
}

/// [`wire`] over an explicit global directory: the testable form.
pub fn wire_at(
    global: Option<&Path>,
    provider: Arc<dyn ModelProvider>,
    config: &mut CodingAgentConfig,
) {
    let defs = discover(global, &config.workspace);
    if defs.is_empty() {
        return;
    }
    let shell = config
        .tools
        .clone()
        .unwrap_or_else(|| Arc::new(ShellTools::new(config.workspace.clone())));
    let tools = SubagentTools::new(provider, defs, shell.clone(), config.model.clone());
    config.tools = Some(shell);
    config.extra_resolver = Some(Arc::new(tools));
}

/// The specs discovered for `workspace` — empty when none are. A prompt
/// preview passes this to `stamped_system` so the preview matches what
/// turns send.
pub fn extra_specs(workspace: &Path) -> Vec<ToolSpec> {
    let global = rhema_config::Config::dir().ok();
    extra_specs_at(global.as_deref(), workspace)
}

/// [`extra_specs`] over an explicit global directory: the testable form.
pub fn extra_specs_at(global: Option<&Path>, workspace: &Path) -> Vec<ToolSpec> {
    let defs = discover(global, workspace);
    if defs.is_empty() {
        Vec::new()
    } else {
        vec![SubagentTools::spec(&defs)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use rhema_coding_agent::stamped_system;
    use rhema_core::{EventSink, Message, ModelOutput, ProviderError, Role, StopReason};

    struct Never;

    #[async_trait::async_trait]
    impl ModelProvider for Never {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "never"),
                stop_reason: StopReason::EndTurn,
                usage: rhema_core::Usage::default(),
            })
        }
    }

    /// Writes one definition folder under `dir`.
    fn def_in(dir: &Path, name: &str, description: &str) {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(crate::def::FILE),
            format!("---\ndescription: {description}\n---\n\nBody of {name}.\n"),
        )
        .unwrap();
    }

    fn workspace_with_defs() -> (tempfile::TempDir, PathBuf) {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        def_in(global.path(), "reviewer", "reviews diffs");
        let root = workspace.path().to_path_buf();
        (global, root)
    }

    #[test]
    fn an_empty_set_of_definitions_leaves_the_config_alone() {
        let empty = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = CodingAgentConfig::new("m", workspace.path());
        wire_at(Some(empty.path()), Arc::new(Never), &mut config);
        assert!(config.tools.is_none());
        assert!(config.extra_resolver.is_none());
        assert!(extra_specs_at(Some(empty.path()), workspace.path()).is_empty());
    }

    #[test]
    fn definitions_attach_the_tool_and_reach_the_prompt() {
        let (global, workspace) = workspace_with_defs();
        let mut config = CodingAgentConfig::new("parent-model", &workspace);
        wire_at(Some(global.path()), Arc::new(Never), &mut config);

        // The shell is shared with the tool: that is the approval
        // inheritance path.
        let shell = config.tools.clone().expect("the shared shell");
        let extra = config.extra_resolver.clone().expect("the tool");
        let names: Vec<String> = extra.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["subagent".to_owned()]);
        assert_eq!(shell.workspace(), workspace);

        // The same discovery feeds the prompt preview.
        let specs = extra_specs_at(Some(global.path()), &workspace);
        assert_eq!(specs.len(), 1);
        let prompt = stamped_system(None, &workspace, &specs).unwrap();
        assert!(prompt.contains("subagent"), "{prompt}");
        assert!(prompt.contains("reviews diffs"), "{prompt}");
    }
}
