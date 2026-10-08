//! The system prompt stamped into requests: the default names the
//! workspace; `CodingAgentConfig::system` replaces it wholesale.

use std::sync::{Arc, Mutex, PoisonError};

use rhema_coding_agent::{AllowAll, CodingAgent, CodingAgentConfig, default_system};
use rhema_core::{
    Event, EventSink, Message, ModelOutput, ModelProvider, ProviderError, Role, StopReason, Usage,
    async_trait,
};

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// Records the `system` field of every request; answers with a fixed
/// end-turn output.
struct SystemRecorder {
    reply: ModelOutput,
    systems: Mutex<Vec<Option<String>>>,
}

impl SystemRecorder {
    fn systems(&self) -> Vec<Option<String>> {
        self.systems
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for SystemRecorder {
    async fn complete(
        &self,
        req: rhema_core::ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.systems
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req.system);
        Ok(self.reply.clone())
    }
}

fn recorder() -> Arc<SystemRecorder> {
    Arc::new(SystemRecorder {
        reply: ModelOutput {
            message: Message::text(Role::Assistant, "ok"),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
        systems: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn the_default_system_prompt_names_the_canonical_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    let provider = recorder();
    let mut agent = CodingAgent::new(
        provider.clone(),
        CodingAgentConfig::new("mock-model", root.path()),
    )
    .unwrap();
    let mut sink = NullSink;

    agent
        .turn("hi", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    let systems = provider.systems();
    assert_eq!(systems.len(), 1);
    // The skills section may follow the base prompt — the machine's own
    // skills — but the base, with the canonical workspace, always leads.
    let system = systems[0].as_deref().expect("one system prompt recorded");
    assert!(
        system.starts_with(default_system(&workspace).as_str()),
        "{system}"
    );
}

#[tokio::test]
async fn a_base_system_prompt_replaces_the_default_and_fills_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    let provider = recorder();
    let config = CodingAgentConfig {
        base_system: Some("BASE in {workspace}.".into()),
        ..CodingAgentConfig::new("mock-model", root.path())
    };
    let mut agent = CodingAgent::new(provider.clone(), config).unwrap();
    let mut sink = NullSink;

    agent
        .turn("hi", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    let systems = provider.systems();
    assert_eq!(systems.len(), 1);
    let system = systems[0].as_deref().expect("one system prompt recorded");
    assert!(
        system.starts_with(&format!("BASE in {}.", workspace.display())),
        "{system}"
    );
    assert!(
        !system.contains("{workspace}"),
        "the placeholder filled: {system}"
    );
}

#[tokio::test]
async fn a_base_system_with_a_skills_slot_places_the_table_there() {
    let root = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    let folder = root.path().join(".rhema").join("skills").join("local");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        "---\ndescription: the local trigger\n---\nbody",
    )
    .unwrap();
    let provider = recorder();
    let config = CodingAgentConfig {
        base_system: Some("HEAD\n{skills}\nTAIL {workspace}".into()),
        ..CodingAgentConfig::new("mock-model", root.path())
    };
    let mut agent = CodingAgent::new(provider.clone(), config).unwrap();
    let mut sink = NullSink;

    agent
        .turn("hi", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    let systems = provider.systems();
    let system = systems[0].as_deref().expect("one system prompt recorded");
    let head = system.find("HEAD").expect("HEAD kept");
    let table = system
        .find("Skills (name / trigger / file)")
        .expect("the skills table present");
    let tail = system.find("TAIL").expect("TAIL kept");
    assert!(head < table && table < tail, "{system}");
    assert!(
        system.contains("local / the local trigger"),
        "the workspace skill is in the table: {system}"
    );
    assert!(
        system.contains(&format!("TAIL {}", workspace.display())),
        "the workspace filled after the table: {system}"
    );
}

#[tokio::test]
async fn config_system_replaces_the_default() {
    let root = tempfile::tempdir().unwrap();
    let provider = recorder();
    let config = CodingAgentConfig {
        system: Some("custom prompt".into()),
        ..CodingAgentConfig::new("mock-model", root.path())
    };
    let mut agent = CodingAgent::new(provider.clone(), config).unwrap();
    let mut sink = NullSink;

    agent
        .turn("hi", &mut sink, Arc::new(AllowAll))
        .await
        .unwrap();

    assert_eq!(provider.systems(), vec![Some("custom prompt".to_string())]);
}
