//! The wired whole: a `CodingAgent` over a steered config runs a turn
//! that starts a subagent, the notification lands at the next turn's
//! start, and the fetch returns the cached report — the exact loop
//! SPEC.md §13 describes, offline.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rhema_agent_tools::{AllowAll, ShellTools};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig};
use rhema_core::{
    Content, Event, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, Usage,
};
use rhema_subagents::{SubagentDef, SubagentTools};

/// Serves the parent agent and the background job off one provider,
/// distinguished by system prompt: the parent (whose system names the
/// marker) answers `parent_replies` in order; the job ends with
/// `job_report` at once.
struct ByRole {
    marker: String,
    parent_replies: Vec<ModelOutput>,
    job_report: &'static str,
    parent_served: AtomicUsize,
}

#[async_trait]
impl ModelProvider for ByRole {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        if req.system.as_deref().unwrap_or("").contains(&self.marker) {
            let round = self.parent_served.fetch_add(1, Ordering::SeqCst);
            return Ok(self
                .parent_replies
                .get(round)
                .cloned()
                .unwrap_or_else(|| self.parent_replies.last().cloned().unwrap()));
        }
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, self.job_report),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}

fn tool_use(id: &str, args: serde_json::Value) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "subagent".into(),
                args,
                ..ToolCall::default()
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

fn end_turn(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

#[tokio::test]
async fn a_wired_agent_starts_notifies_and_fetches_by_job_id() {
    let provider = Arc::new(ByRole {
        marker: "the parent's system prompt".into(),
        // Round 0: start a scout. Round 1: end the turn — the job may
        // or may not have finished by the between-steps drain, so the
        // notification's landing is asserted on the session, not here.
        // Round 2: fetch by id. Round 3: end.
        parent_replies: vec![
            tool_use(
                "c1",
                serde_json::json!({"name": "scout", "prompt": "look around"}),
            ),
            end_turn("started it"),
            tool_use("c2", serde_json::json!({"id": "job-1"})),
            end_turn("done"),
        ],
        job_report: "scout report",
        parent_served: AtomicUsize::new(0),
    });
    let scout = SubagentDef {
        name: "scout".into(),
        description: "looks around".into(),
        model: None,
        max_steps: None,
        body: "You are the scout. Look.".into(),
    };
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = Arc::new(
        SubagentTools::new(provider.clone(), vec![scout], shell.clone(), "parent-model")
            .with_thinking(rhema_core::request::ThinkingEffort::Default),
    );
    // The wire's shape: the tool resolver and the steering read the one
    // registry.
    let mut config = CodingAgentConfig::new("m", std::env::temp_dir());
    // The marker the provider tells the parent's requests by: the base
    // prompt rides into the rendered system.
    config.base_system = Some("the parent's system prompt".into());
    config.tools = Some(shell);
    config.extra_resolver = Some(tools.clone());
    config.steering = Some(tools.handle().steering());
    let mut agent = CodingAgent::new(provider.clone(), config).unwrap();

    // Turn 1: the start call answers in-band with a job id; the job
    // runs in the background, across the turn's end.
    agent
        .turn("delegate the look", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();
    let start_results: Vec<String> = agent
        .session()
        .history
        .iter()
        .filter_map(|message| match &message.content[0] {
            Content::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        start_results
            .iter()
            .any(|content| content.contains("job-1") && content.contains("started")),
        "the start names its job: {start_results:?}"
    );

    // The job finishes while no turn runs.
    let deadline = Instant::now() + Duration::from_secs(5);
    while tools
        .handle()
        .status()
        .first()
        .is_none_or(|job| job.phase != "finished")
    {
        assert!(Instant::now() < deadline, "the job never finished");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Turn 2: the notification precedes the input; the fetch returns
    // the cached report.
    agent
        .turn("get it", &mut NullSink, Arc::new(AllowAll))
        .await
        .unwrap();
    let history = &agent.session().history;
    let is_notification = |message: &Message| {
        message.role == Role::User
            && message
                .content
                .iter()
                .any(|content| matches!(content, Content::Text(text) if text.contains("Subagent scout (job-1) has finished.")))
    };
    assert_eq!(
        history.iter().filter(|m| is_notification(m)).count(),
        1,
        "one notification message: {:?}",
        history
    );
    // The notification preceded the user's input; consecutive User
    // messages are valid.
    let notification_pos = history.iter().position(is_notification).unwrap();
    let input_pos = history
        .iter()
        .rposition(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .any(|c| matches!(c, Content::Text(text) if text == "get it"))
        })
        .unwrap();
    assert!(
        notification_pos < input_pos,
        "announced before the input: {history:?}"
    );
    // The fetch returned the report the job produced once.
    assert!(
        history.iter().any(|message| {
            message.role == Role::Tool
                && message
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::ToolResult(result) if result.content == "scout report"))
        }),
        "the fetch carries the report: {:?}",
        history
    );
}
