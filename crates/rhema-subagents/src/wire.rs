//! The explicit wiring: frontends call [`wire`], the driver never
//! discovers subagents on its own.
//!
//! [`wire`] discovers definitions and attaches a [`SubagentTools`] to
//! a `HostAgentConfig`, the shared shell, the extra resolver the
//! driver merges through its `Overlay`, and the steering: the tool's
//! registry is the driver's notification source, so a finished job is
//! announced to the model by the very instance that started it. The
//! same call hands back a [`SubagentHandle`], the registry-backed
//! status subscription a frontend's redraw tick renders one live line
//! per job from. The catalog is never empty unless the config's
//! disabled set says so: the built-in `general-purpose` definition is
//! always discovered, discovered ones beside or shadowing it, and the
//! config's `disabled_subagents` names drop before the catalog
//! builds, a set holding every name leaves no catalog, and the
//! config keeps its default wiring. [`extra_specs`] feeds the same
//! filtered discovery to a prompt preview, so what is previewed is
//! what turns send.

use std::path::Path;
use std::sync::Arc;

use rhema_bash::ShellTools;
use rhema_core::ToolSpec;
use rhema_host_agent::HostAgentConfig;

use crate::def::discover;
use crate::registry::Registry;
use crate::tools::SubagentTools;

/// The registry-backed handle `wire` returns beside the config: the
/// UI's status subscription, the finish notice feed, the idle
/// notification take, and the driver's steering, all reading the one
/// registry the `subagent` tool starts jobs into. Cloneable; a clone
/// reads the same jobs.
#[derive(Clone)]
pub struct SubagentHandle {
    registry: Arc<Registry>,
    /// The tool that starts into the registry, held weakly like the
    /// jobs hold their registry: the handle survives the tool, and a
    /// restart past the drop is an empty answer, never a spawn into a
    /// gone agent. [`Self::from_registry`] builds the registry-only
    /// shape with no tool behind it.
    tools: std::sync::Weak<SubagentTools>,
}

impl SubagentHandle {
    /// The tool's handle constructor: one registry, one source. The
    /// registry-only form (`from_registry`) carries no tool, so its
    /// [`Self::restart_failed_jobs`] always restarts nothing.
    pub(crate) fn from_registry(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            tools: std::sync::Weak::new(),
        }
    }

    /// The handle over `tools`: its registry, and the weak back-link a
    /// restart spawns through.
    pub(crate) fn from_tools(tools: &std::sync::Arc<SubagentTools>) -> Self {
        Self {
            registry: tools.registry(),
            tools: std::sync::Arc::downgrade(tools),
        }
    }

    /// Attaches the store a child session is saved under and the parent
    /// it belongs to: the frontend calls this once it knows which
    /// conversation it opened. Before it, a subagent runs in memory and
    /// leaves no child behind.
    pub fn attach_store(
        &self,
        store: Arc<dyn rhema_session_store::SessionStore>,
        parent: rhema_session_store::SessionId,
    ) {
        if let Some(tools) = self.tools.upgrade() {
            tools.attach_store(store, parent);
        }
    }

    /// Notes a finish the registry did not observe: a child that ended
    /// while the parent was down. It reaches the transcript notices and
    /// the notification feed, once each.
    pub fn note_finished(&self, finished: crate::registry::JobFinished) {
        self.registry.note_finished(finished);
    }

    /// The starts the parent's ledger has not recorded yet: the child
    /// session and the recipe a record needs. The frontend submits them
    /// into the session and saves once. Empty after a drain.
    pub fn take_starts(&self) -> Vec<crate::registry::Started> {
        self.registry.take_starts()
    }

    /// The jobs' screen-facing state: one entry per working job,
    /// queued or running, oldest first. A finished job left at once;
    /// its outcome arrived on [`Self::take_finished`]. Empty when the
    /// session wired no subagents.
    pub fn status(&self) -> Vec<crate::registry::JobStatus> {
        self.registry.status()
    }

    /// Takes the spend the frontend has not priced yet: per model id,
    /// the tokens the jobs' steps added since the previous take. The
    /// frontend prices each entry by that model's card and adds the
    /// result to the session's cost. Empty after a drain.
    pub fn take_spend(&self) -> Vec<(String, rhema_core::Usage)> {
        self.registry.take_spend()
    }

    /// The finish events, oldest first, one per job, exactly once per
    /// job. The transcript notice feed: a frontend pushes each event's
    /// `line` the moment the job ends, whatever turn is running.
    pub fn take_finished(&self) -> Vec<crate::registry::JobFinished> {
        self.registry.take_finished()
    }

    /// Takes the pending notification lines out, oldest first, marking
    /// them announced. The idle-session path: a frontend with no
    /// running turn runs the lines as the next turn's prompt, the
    /// regular-prompt delivery, and this take keeps the driver's
    /// turn-start drain from repeating what the prompt already said.
    /// While a turn runs, leave these for the driver's steering.
    pub fn take_notifications(&self) -> Vec<String> {
        self.registry.take_notifications()
    }

    /// Takes the children whose finishes the frontend delivered — a
    /// prompt line taken, a report fetched — with the outcome the
    /// ledger record settles to. Drained beside the session writes,
    /// so a finish the model answered never reads at a load as one
    /// the process died before it could announce. Empty after a drain.
    pub fn take_settled(&self) -> Vec<crate::registry::Settled> {
        self.registry.take_settled()
    }

    /// The registry as the driver's steering source: one notification
    /// line per finished-but-untaken job, per drain. Set this on
    /// `HostAgentConfig::steering`, it is already set by [`wire`];
    /// a frontend rebuilding configs by hand carries it over.
    pub fn steering(&self) -> Arc<dyn rhema_host_agent::Steering> {
        Arc::clone(&self.registry) as Arc<dyn rhema_host_agent::Steering>
    }

    /// Restarts every transient-failed job in place, the retry
    /// command's subagent half. A transient failure names the
    /// infrastructure (a dead link, a struggling server): the job
    /// carried everything it needs to run again, so it re-runs under
    /// its own id, from its start, with a fresh cancel flag. The
    /// restarts come back so a frontend can notice them; each re-run
    /// announces itself when it finishes, into the id the model
    /// already knows. Permanent failures, no report, a spent step
    /// budget, stay failed.
    pub fn restart_failed_jobs(&self) -> Vec<crate::registry::Restarted> {
        match self.tools.upgrade() {
            Some(tools) => tools.restart_failed_jobs(),
            None => Vec::new(),
        }
    }
}

/// Wires discovered subagents into `config`, reading the global
/// definitions directory off the config dir. See [`wire_at`].
pub fn wire(mut config: HostAgentConfig) -> (HostAgentConfig, SubagentHandle) {
    let global = rhema_config::Home::discover().ok().map(|h| h.subagents());
    let handle = wire_at(global.as_deref(), &mut config);
    (config, handle)
}

/// [`wire`] over an explicit global directory: the testable form.
/// Attaches the tool, the steering, and returns the status handle,
/// all backed by the one registry. The tool rides the config's own
/// provider, one instance for the session and its subagents.
pub fn wire_at(global: Option<&Path>, config: &mut HostAgentConfig) -> SubagentHandle {
    let mut defs = discover(global, &config.workspace);
    // The persisted off switch runs here, before the catalog builds:
    // a disabled name is in neither the prompt's tool list nor the
    // tools array, and the built-in answers to its folder name like
    // any other definition.
    defs.retain(|def| !config.disabled_subagents.contains(&def.name));
    if defs.is_empty() {
        // Reachable only when every name, the built-in included,
        // sits in the disabled set; the config stays untouched rather
        // than advertising an empty catalog.
        return SubagentHandle::from_registry(Arc::new(Registry::new(
            crate::registry::DEFAULT_MAX_RUNNING,
        )));
    }
    // The shared shell carries the config's env, the wiring's
    // provisioned BASH_ENV and socket ride it. Building `ShellTools::new`
    // here instead would strand the env in a config field nothing reads:
    // agent.rs's own `with_env` fallback never runs once `tools` is
    // `Some`, and every bash command would lose the functions the prompt
    // advertises. (Regression shape: the prompt listed the MCP servers
    // while `mcp` was `command not found` in the same session.)
    let shell = config.tools.clone().unwrap_or_else(|| {
        Arc::new(ShellTools::with_env(
            config.workspace.clone(),
            &config.shell_env,
        ))
    });
    let tools = Arc::new(
        SubagentTools::new(
            config.provider.clone(),
            defs,
            shell.clone(),
            config.model.clone(),
        )
        .with_thinking(config.thinking)
        .with_base_system(subagent_base_system(config, shell.workspace()))
        // The frontend's credential answers, when it carries any:
        // the `model` argument's `<provider>/<model>` spellings
        // resolve through these, and the spec's provider list is
        // their wire-time snapshot.
        .with_providers_if(config.subagent_providers.clone()),
    );
    let handle = tools.handle();
    config.tools = Some(shell);
    // An extension already in either slot stays reachable: the
    // resolver chains through `Overlay` and the steering through
    // `SteeringChain`. Overwriting them used to silence whichever
    // frontend wired its extension first.
    let resolver: Arc<dyn rhema_core::ToolResolver> = tools;
    config.extra_resolver = Some(match config.extra_resolver.take() {
        Some(existing) => Arc::new(rhema_core::Overlay::new(existing, resolver)),
        None => resolver,
    });
    let steering = handle.steering();
    config.steering = Some(match config.steering.take() {
        Some(existing) => Arc::new(rhema_host_agent::SteeringChain::new(vec![
            existing, steering,
        ])),
        None => steering,
    });
    handle
}

/// The system prompt a subagent runs under: the parent's full stamp,
/// the base template rendered over `workspace`, its skills (the
/// disabled dropped), the environment facts, the user preferences,
/// with the tool list the subagent actually carries (the shell; a
/// nested `subagent` tool arrives only by depth and rides the
/// request's tools array). `config.system`, the total-replacement
/// escape hatch, replaces the stamp whole, exactly as it replaces the
/// parent's. A stamp that cannot render (a broken base template)
/// warns and leaves the bodies bare: the parent's own construction
/// fails on the same template, so the session is already leaving.
fn subagent_base_system(config: &HostAgentConfig, workspace: &Path) -> String {
    if let Some(system) = &config.system {
        return system.clone();
    }
    let mut skills = match &config.skills {
        Some(skills) => skills.clone(),
        None => {
            let home = config
                .home
                .clone()
                .or_else(|| rhema_config::Home::discover().ok());
            let agents = home.as_ref().map(|h| h.agents_skills());
            let global = home.as_ref().map(|h| h.skills());
            rhema_skills::discover(agents.as_deref(), global.as_deref(), workspace)
        }
    };
    skills.retain(|skill| !config.disabled_skills.contains(&skill.name));
    let specs = ShellTools::builtin_specs();
    // The subagent's own prompt carries no vision section: the function
    // rides its env either way, and the section documents the parent
    // shell's tool surface. Unlike vision, jobs does belong here: a
    // subagent's whole point is long shell work, and the parent wired
    // the function into the env this prompt's shell also runs with. No
    // MCP section either: a subagent inherits the functions, not the
    // disclosure.
    let sections = [
        ("environment", config.environment.as_deref()),
        ("user", config.user_preferences.as_deref()),
        ("jobs", config.jobs.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, text)| text.map(|text| (name, serde_json::json!(text))))
    .collect::<Vec<_>>();
    rhema_host_agent::stamped_system_with_sections(
        config.base_system.as_deref(),
        workspace,
        &specs,
        &skills,
        &sections,
    )
    .unwrap_or_else(|err| {
        eprintln!(
            "· subagents: stamping the system prompt failed ({err}); the definitions run bare"
        );
        String::new()
    })
}

/// The specs discovered for `workspace` minus the disabled names,
/// empty when none survive. A prompt preview passes this to
/// `stamped_system` so the preview matches what turns send; `disabled`
/// is the config's persisted off switch, the same set the wire filters
/// by, and `provider_kinds` the providers a `model` argument can
/// reach, the factory's wire-time answers. A preview that touches no
/// credentials (the `system show` path) passes an empty slice and its
/// spec names no providers.
pub fn extra_specs(
    workspace: &Path,
    disabled: &std::collections::BTreeSet<String>,
    provider_kinds: &[&str],
) -> Vec<ToolSpec> {
    let global = rhema_config::Home::discover().ok().map(|h| h.subagents());
    extra_specs_at(global.as_deref(), workspace, disabled, provider_kinds)
}

/// [`extra_specs`] over an explicit global directory: the testable form.
pub fn extra_specs_at(
    global: Option<&Path>,
    workspace: &Path,
    disabled: &std::collections::BTreeSet<String>,
    provider_kinds: &[&str],
) -> Vec<ToolSpec> {
    let mut defs = discover(global, workspace);
    defs.retain(|def| !disabled.contains(&def.name));
    if defs.is_empty() {
        Vec::new()
    } else {
        vec![SubagentTools::spec(&defs, provider_kinds)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use rhema_core::{
        EventSink, Message, ModelOutput, ModelProvider, ProviderError, Role, StopReason,
    };
    use rhema_host_agent::stamped_system;

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

    #[tokio::test]
    async fn the_wire_carries_the_provider_factory_into_the_tool() {
        use rhema_core::{ToolCall, ToolResolver};
        let empty = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        def_in(
            workspace.path().join(".rhema").join("subagents").as_path(),
            "scout",
            "scouts ahead",
        );
        let mut config = HostAgentConfig::new(
            Arc::new(Never),
            "m",
            workspace.path(),
            rhema_host_agent::AllowAll,
        );
        // The factory answers only anthropic; the model the wire's
        // config carries is never the parent here.
        let other = Arc::new(Never);
        config.subagent_providers = Some(Arc::new(move |kind: &str| {
            (kind == "anthropic").then(|| other.clone() as Arc<dyn ModelProvider>)
        }) as rhema_host_agent::SubagentProviders);
        let _handle = wire_at(Some(empty.path()), &mut config);
        let resolver = config.extra_resolver.as_ref().expect("the tool is wired");
        let spec = &ToolResolver::specs(resolver.as_ref())[0];
        // The spec's taught list is the factory's wire-time answers.
        assert!(
            spec.description.contains("available here: anthropic"),
            "{}",
            spec.description
        );
        // And a start whose kind the factory cannot resolve fails
        // in-band naming it, never a silent ride on the session's
        // provider.
        let ctx = rhema_core::ToolContext {
            cwd: workspace.path().to_path_buf(),
            run_id: "turn-1".into(),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let call = ToolCall {
            id: "c1".into(),
            name: "subagent".into(),
            args: serde_json::json!({
                "name": "scout",
                "prompt": "go",
                "model": "openrouter/no-such-login"
            }),
            provider_data: serde_json::Value::Null,
        };
        let result = resolver.resolve(call, &ctx).await;
        assert!(result.is_error, "an unresolvable kind must fail the call");
        assert!(result.content.contains("openrouter"), "{}", result.content);
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

    #[tokio::test]
    async fn the_shared_shell_carries_the_config_env() {
        use rhema_core::{ToolCall, ToolResolver};
        // The regression shape: the config's shell_env (the MCP wiring's
        // provisioned BASH_ENV and socket among it) must reach the bash
        // tool's children. wire_at used to rebuild the shell without it,
        // and agent.rs's own with_env fallback never ran because tools
        // was already Some, the prompt advertised functions the shell
        // had never heard of.
        let empty = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = HostAgentConfig::new(
            Arc::new(Never),
            "m",
            workspace.path(),
            rhema_host_agent::AllowAll,
        );
        config
            .shell_env
            .insert("RHEMA_WIRE_TEST".to_owned(), "seen".to_owned());
        let handle = wire_at(Some(empty.path()), &mut config);
        let shell = config.tools.clone().expect("the shared shell");
        shell.install_approval(std::sync::Arc::new(rhema_bash::AllowAll));
        let result = ToolResolver::resolve(
            shell.as_ref(),
            ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "printf %s $RHEMA_WIRE_TEST" }),
                provider_data: serde_json::Value::Null,
            },
            &rhema_core::tool::ToolContext::default(),
        )
        .await;
        assert!(
            result.content.contains("seen"),
            "the env reached the child: {result:?}"
        );
        let _ = handle;
    }

    #[test]
    fn nothing_on_disk_still_wires_the_builtin() {
        let empty = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        // The shell canonicalizes, so the assertion compares against the
        // canonical form of the same directory.
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let mut config =
            HostAgentConfig::new(Arc::new(Never), "m", &root, rhema_host_agent::AllowAll);
        let handle = wire_at(Some(empty.path()), &mut config);

        // The catalog is never empty: the built-in general-purpose
        // definition is the tool even with nothing on disk.
        let shell = config.tools.clone().expect("the shared shell");
        let extra = config.extra_resolver.clone().expect("the tool");
        let names: Vec<String> = extra.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["subagent".to_owned()]);
        assert!(
            extra.specs()[0].description.contains("general-purpose"),
            "{}",
            extra.specs()[0].description
        );
        assert_eq!(shell.workspace(), root);
        assert_eq!(
            extra_specs_at(
                Some(empty.path()),
                workspace.path(),
                &Default::default(),
                &[]
            )
            .len(),
            1,
            "the preview sees the same built-in"
        );
        assert!(handle.status().is_empty(), "no jobs yet");
    }

    #[test]
    fn a_user_definition_shadows_the_builtin_in_the_catalog() {
        let (global, workspace) = workspace_with_defs();
        def_in(global.path(), "general-purpose", "my own generalist");
        let specs = extra_specs_at(Some(global.path()), &workspace, &Default::default(), &[]);
        let description = &specs[0].description;
        assert!(description.contains("my own generalist"), "{description}");
        assert!(
            !description.contains("Runs one self-contained"),
            "{description}"
        );
    }

    #[test]
    fn definitions_attach_the_tool_and_reach_the_prompt() {
        let (global, workspace) = workspace_with_defs();
        let mut config = HostAgentConfig::new(
            Arc::new(Never),
            "parent-model",
            &workspace,
            rhema_host_agent::AllowAll,
        );
        let handle = wire_at(Some(global.path()), &mut config);

        // The shell is shared with the tool: that is the approval
        // inheritance path.
        let shell = config.tools.clone().expect("the shared shell");
        let extra = config.extra_resolver.clone().expect("the tool");
        let names: Vec<String> = extra.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["subagent".to_owned()]);
        assert_eq!(shell.workspace(), workspace);

        // The steering is the registry's: the driver announces what the
        // tool starts, from the one instance.
        let steering = config.steering.clone().expect("the steering");
        assert!(steering.drain().is_empty(), "no jobs, no notifications");

        // The same discovery feeds the prompt preview.
        let specs = extra_specs_at(Some(global.path()), &workspace, &Default::default(), &[]);
        assert_eq!(specs.len(), 1);
        let prompt = stamped_system(None, &workspace, &specs).unwrap();
        assert!(prompt.contains("subagent"), "{prompt}");
        assert!(prompt.contains("reviews diffs"), "{prompt}");

        // The handle reads the same registry the config steers with.
        assert!(handle.status().is_empty());
        let _ = handle.steering().drain();
    }

    #[test]
    fn the_disabled_set_drops_definitions_before_the_catalog_builds() {
        let (global, workspace) = workspace_with_defs();

        // The preview: a disabled name is in no spec. The built-in is
        // always discovered too, so "every name" includes it.
        let specs = extra_specs_at(
            Some(global.path()),
            &workspace,
            &BTreeSet::from(["reviewer".to_owned(), "general-purpose".to_owned()]),
            &[],
        );
        assert!(specs.is_empty(), "{specs:?}");

        let specs = extra_specs_at(
            Some(global.path()),
            &workspace,
            &BTreeSet::from(["reviewer".to_owned()]),
            &[],
        );
        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].description.contains("general-purpose"),
            "{}",
            specs[0].description
        );

        def_in(global.path(), "scout", "scouts ahead");
        let specs = extra_specs_at(
            Some(global.path()),
            &workspace,
            &BTreeSet::from(["reviewer".to_owned()]),
            &[],
        );
        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].description.contains("scouts ahead"),
            "{}",
            specs[0].description
        );
        assert!(
            !specs[0].description.contains("reviews diffs"),
            "{}",
            specs[0].description
        );

        // The wire: the config's own set runs the same filter, the
        // built-in answering to its name like any other.
        def_in(global.path(), "general-purpose", "my generalist");
        let mut config =
            HostAgentConfig::new(Arc::new(Never), "m", &workspace, rhema_host_agent::AllowAll);
        config.disabled_subagents = BTreeSet::from([
            "reviewer".to_owned(),
            "scout".to_owned(),
            "general-purpose".to_owned(),
        ]);
        let handle = wire_at(Some(global.path()), &mut config);
        assert!(
            config.extra_resolver.is_none(),
            "an empty catalog leaves the config unwired"
        );
        assert!(handle.status().is_empty());
        let _ = handle.steering().drain();

        // Disabling one of two leaves the other: the tool stays.
        let mut config =
            HostAgentConfig::new(Arc::new(Never), "m", &workspace, rhema_host_agent::AllowAll);
        config.disabled_subagents = BTreeSet::from(["reviewer".to_owned()]);
        let _handle = wire_at(Some(global.path()), &mut config);
        let extra = config.extra_resolver.clone().expect("the tool");
        let description = extra.specs()[0].description.clone();
        assert!(description.contains("scouts ahead"), "{description}");
        assert!(
            description.contains("my generalist"),
            "the unlisted built-in shadow stays: {description}"
        );
        assert!(!description.contains("reviews diffs"), "{description}");
    }

    #[test]
    fn the_subagent_stamp_carries_the_parents_sections() {
        let workspace = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let mut config =
            HostAgentConfig::new(Arc::new(Never), "m", &root, rhema_host_agent::AllowAll);
        config.environment = Some("ENVSAYSHI".into());
        config.user_preferences = Some("USERSAYSHI".into());
        config.skills = Some(vec![rhema_prompt::Skill {
            name: "skill".into(),
            description: "does a thing".into(),
            license: None,
            path: root.join("SKILL.md"),
            tier: rhema_prompt::SkillTier::Workspace,
            body: "the rules".into(),
        }]);

        let stamp = subagent_base_system(&config, &root);
        assert!(stamp.contains("ENVSAYSHI"), "{stamp}");
        assert!(stamp.contains("USERSAYSHI"), "{stamp}");
        assert!(stamp.contains("does a thing"), "{stamp}");
        assert!(stamp.contains(&root.display().to_string()), "{stamp}");
        // The tool list is the subagent's own surface: the shell, not
        // the parent's extensions.
        assert!(stamp.contains("- bash |"), "{stamp}");
        assert!(!stamp.contains("- zoom |"), "{stamp}");
        assert!(!stamp.contains("- subagent |"), "{stamp}");
    }

    #[test]
    fn a_total_replacement_replaces_the_subagent_stamp_too() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = HostAgentConfig::new(
            Arc::new(Never),
            "m",
            workspace.path(),
            rhema_host_agent::AllowAll,
        );
        config.system = Some("the replacement".into());
        assert_eq!(
            subagent_base_system(&config, workspace.path()),
            "the replacement"
        );
    }
}

#[cfg(test)]
mod extension_chain_tests {
    use super::*;

    struct Never;

    #[async_trait::async_trait]
    impl rhema_core::ModelProvider for Never {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn rhema_core::EventSink,
        ) -> Result<rhema_core::ModelOutput, rhema_core::ProviderError> {
            Ok(rhema_core::ModelOutput {
                message: rhema_core::Message::text(rhema_core::Role::Assistant, "never"),
                stop_reason: rhema_core::StopReason::EndTurn,
                usage: rhema_core::Usage::default(),
            })
        }
    }

    /// A resolver another extension already installed.
    struct Existing;

    #[async_trait::async_trait]
    impl rhema_core::ToolResolver for Existing {
        fn specs(&self) -> Vec<rhema_core::ToolSpec> {
            vec![rhema_core::ToolSpec::new(
                "search",
                "the other extension's tool",
                serde_json::json!({ "type": "object" }),
            )]
        }

        async fn resolve(
            &self,
            call: rhema_core::ToolCall,
            _ctx: &rhema_core::ToolContext,
        ) -> rhema_core::ToolResult {
            rhema_core::ToolResult::success(call.id, "from the other extension")
        }
    }

    /// The other extension's steering feed.
    struct ExistingSteering;

    impl rhema_host_agent::Steering for ExistingSteering {
        fn drain(&self) -> Vec<String> {
            vec!["the other extension finished".to_owned()]
        }
    }

    /// A config that already carries an extension must keep it: the
    /// subagent wire composes, it does not overwrite.
    #[test]
    fn wiring_the_subagent_keeps_an_earlier_extensions_resolver_and_steering() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut config = HostAgentConfig::new(
            Arc::new(Never),
            "m",
            workspace.path(),
            rhema_host_agent::AllowAll,
        );
        config.extra_resolver = Some(Arc::new(Existing));
        config.steering = Some(Arc::new(ExistingSteering));

        let handle = wire_at(Some(global.path()), &mut config);

        let after = config.extra_resolver.clone().expect("still installed");
        let names: Vec<String> = after.specs().into_iter().map(|spec| spec.name).collect();
        assert!(names.contains(&"search".to_owned()), "{names:?}");
        assert!(names.contains(&"subagent".to_owned()), "{names:?}");
        // Both steering feeds drain, the earlier one first.
        let drained = config.steering.as_ref().expect("chained").drain();
        assert_eq!(drained, vec!["the other extension finished".to_owned()]);
        let _ = handle;
    }
}
