//! The `subagent` tool end-to-end over a scripted provider: report
//! round-trip, in-band failure, approval inheritance, nesting, and
//! cancellation.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use rhema_agent_tools::{AllowAll, Approval, ShellTools};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Content, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
    StopReason, ToolCall, ToolContext, ToolResolver, Usage,
};
use rhema_subagents::{SubagentDef, SubagentTools};

/// An end-turn reply with `text`.
fn end_turn(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// A tool-use reply calling `name` with `args`.
fn tool_use(id: &str, name: &str, args: serde_json::Value) -> ModelOutput {
    ModelOutput {
        message: Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                args,
                ..ToolCall::default()
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

/// Serves `replies` in order; the last one repeats. Records every
/// request's tool names and every `Tool`-role result that reached the
/// model, for the assertions below.
struct Scripted {
    replies: Vec<ModelOutput>,
    served: AtomicUsize,
    tool_names: Mutex<Vec<Vec<String>>>,
    tool_results: Mutex<Vec<String>>,
    thinking: Mutex<Vec<ThinkingEffort>>,
}

impl Scripted {
    fn new(replies: Vec<ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            replies,
            served: AtomicUsize::new(0),
            tool_names: Mutex::new(Vec::new()),
            tool_results: Mutex::new(Vec::new()),
            thinking: Mutex::new(Vec::new()),
        })
    }

    fn served_tools(&self) -> Vec<Vec<String>> {
        self.tool_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn results_seen(&self) -> Vec<String> {
        self.tool_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The thinking effort every request carried, in order.
    fn thinkings_seen(&self) -> Vec<ThinkingEffort> {
        self.thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for Scripted {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let index = self.served.fetch_add(1, Ordering::SeqCst);
        self.thinking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.params.thinking);
        self.tool_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        for message in &req.messages {
            for content in &message.content {
                if let Content::ToolResult(result) = content {
                    self.tool_results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(result.content.clone());
                }
            }
        }
        Ok(self
            .replies
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.replies.last().cloned().unwrap()))
    }
}

/// Serves replies that differ by system prompt: the round whose system
/// names `marker` answers `first` once and `then` afterwards,
/// everything else answers `other`. Distinguishes the nested levels of
/// one run, which share the provider but not the prompt.
struct BySystem {
    marker: String,
    first: ModelOutput,
    then: ModelOutput,
    other: ModelOutput,
    marked_seen: AtomicUsize,
}

#[async_trait]
impl ModelProvider for BySystem {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let marked = req.system.as_deref().unwrap_or("").contains(&self.marker);
        if marked {
            let round = self.marked_seen.fetch_add(1, Ordering::SeqCst);
            return Ok(if round == 0 {
                self.first.clone()
            } else {
                self.then.clone()
            });
        }
        Ok(self.other.clone())
    }
}

/// An approval hook that records every call it was asked about.
struct Spy {
    approved: bool,
    calls: Mutex<Vec<String>>,
}

impl Spy {
    fn recorder(approved: bool) -> Arc<Self> {
        Arc::new(Self {
            approved,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn taken(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Approval for Spy {
    fn approve(&self, call: &ToolCall) -> bool {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call.name.clone());
        self.approved
    }
}

/// A definition whose inner agent runs one `echo` and reports.
fn worker_def() -> SubagentDef {
    SubagentDef {
        name: "worker".into(),
        description: "does a unit of work".into(),
        model: None,
        max_steps: None,
        body: "You are the worker. Run the task, then report.".into(),
    }
}

/// A resolver over `worker`, wired to `provider` through a parent shell.
fn worker_tools(provider: Arc<dyn ModelProvider>) -> (Arc<ShellTools>, Arc<dyn ToolResolver>) {
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = SubagentTools::new(provider, vec![worker_def()], shell.clone(), "parent-model");
    (shell, Arc::new(tools))
}

fn call(name: &str, prompt: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({ "name": name, "prompt": prompt }),
        ..ToolCall::default()
    }
}

#[tokio::test]
async fn a_call_runs_the_inner_agent_and_returns_its_report() {
    let provider = Scripted::new(vec![
        tool_use(
            "i1",
            "bash",
            serde_json::json!({"command": "echo subagent-output"}),
        ),
        end_turn("the report"),
    ]);
    let (shell, tools) = worker_tools(provider.clone());
    let spy = Spy::recorder(true);
    shell.install_approval(spy.clone());

    let mut lines = Vec::new();
    let result = tools
        .resolve_streaming(
            call("worker", "do it"),
            &ToolContext::default(),
            &mut |line| lines.push(line.to_owned()),
        )
        .await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "the report");
    // The inner shell inherited the turn's hook: it saw and approved the
    // inner command.
    assert_eq!(spy.taken(), vec!["bash".to_owned()]);
    // The inner command's output rode the progress channel as lines.
    assert!(
        lines.iter().any(|line| line.trim() == "subagent-output"),
        "{lines:?}"
    );
    // The inner turn saw both tools; the inner agent ended after the
    // report, and the parent model saw the inner command's output.
    assert_eq!(
        provider.served_tools()[0],
        vec!["bash".to_owned()],
        "one level of nesting: no subagent tool inside"
    );
    assert!(
        provider
            .results_seen()
            .iter()
            .any(|content| content.contains("subagent-output")),
        "the inner model saw its command's output: {:?}",
        provider.results_seen()
    );
    // Only the inner shell's hook is released here; the parent's stays
    // installed for the rest of its turn, as the parent turn owns it.
    assert!(shell.current_approval().is_some());
}

#[tokio::test]
async fn an_unknown_name_is_an_in_band_error() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let (_shell, tools) = worker_tools(provider);
    let result = tools
        .resolve(call("nobody", "do it"), &ToolContext::default())
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("no subagent named nobody") && result.content.contains("worker"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn bad_arguments_are_in_band_errors() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let (_shell, tools) = worker_tools(provider);
    let ctx = ToolContext::default();

    let missing = tools
        .resolve(
            ToolCall {
                id: "c2".into(),
                name: "subagent".into(),
                args: serde_json::json!({ "name": "worker" }),
                ..ToolCall::default()
            },
            &ctx,
        )
        .await;
    assert!(missing.is_error && missing.content.contains("`prompt`"));

    let empty = tools.resolve(call("worker", "   "), &ctx).await;
    assert!(empty.is_error && empty.content.contains("empty"));
}

#[tokio::test]
async fn the_inner_shell_inherits_a_denial_too() {
    let provider = Scripted::new(vec![
        tool_use("i1", "bash", serde_json::json!({"command": "echo nope"})),
        end_turn("gave up"),
    ]);
    let (shell, tools) = worker_tools(provider.clone());
    let spy = Spy::recorder(false);
    shell.install_approval(spy.clone());

    let result = tools
        .resolve(call("worker", "do it"), &ToolContext::default())
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "gave up");
    // The hook saw the call and declined it; the inner model saw the
    // rejection and reported giving up.
    assert_eq!(spy.taken(), vec!["bash".to_owned()]);
    assert!(
        provider
            .results_seen()
            .iter()
            .any(|content| content.contains("rejected by user approval")),
        "{:?}",
        provider.results_seen()
    );
}

#[tokio::test]
async fn no_hook_installed_rejects_the_inner_tools() {
    let provider = Scripted::new(vec![
        tool_use("i1", "bash", serde_json::json!({"command": "echo nope"})),
        end_turn("blocked"),
    ]);
    let (_shell, tools) = worker_tools(provider.clone());
    let result = tools
        .resolve(call("worker", "do it"), &ToolContext::default())
        .await;
    assert_eq!(result.content, "blocked");
    assert!(
        provider
            .results_seen()
            .iter()
            .any(|content| content.contains("rejected")),
        "the fail-safe default stands: {:?}",
        provider.results_seen()
    );
}

#[tokio::test]
async fn an_exhausted_budget_is_an_in_band_error() {
    let mut def = worker_def();
    def.max_steps = Some(1);
    let provider = Scripted::new(vec![tool_use(
        "i1",
        "bash",
        serde_json::json!({"command": "echo loop"}),
    )]);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    shell.install_approval(Arc::new(AllowAll));
    let tools = Arc::new(SubagentTools::new(provider, vec![def], shell, "m"));

    let result = tools
        .resolve(call("worker", "loop forever"), &ToolContext::default())
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("step budget of 1"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_cancelled_flag_stops_the_run_before_its_next_step() {
    let provider = Scripted::new(vec![tool_use(
        "i1",
        "bash",
        serde_json::json!({"command": "sleep 5"}),
    )]);
    let (_shell, tools) = worker_tools(provider);
    let ctx = ToolContext {
        cancelled: Arc::new(AtomicBool::new(true)),
        ..ToolContext::default()
    };
    let result = tools.resolve(call("worker", "do it"), &ctx).await;
    assert!(
        result.is_error && result.content.contains("cancelled"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn nesting_follows_the_budget() {
    // Budget 1 (the default): the inner agent sees no subagent tool.
    let provider = Scripted::new(vec![end_turn("leaf")]);
    let (_shell, tools) = worker_tools(provider.clone());
    tools.specs();
    let _ = tools
        .resolve(call("worker", "do it"), &ToolContext::default())
        .await;
    assert_eq!(
        provider.served_tools()[0],
        vec!["bash".to_owned()],
        "no subagent tool inside at the default budget"
    );

    // Budget 2: the inner agent sees one, and a nested call resolves.
    let provider = Arc::new(BySystem {
        marker: "You are the worker".to_owned(),
        first: tool_use(
            "n1",
            "subagent",
            serde_json::json!({"name": "scout", "prompt": "look around"}),
        ),
        then: end_turn("outer done"),
        other: end_turn("scout report"),
        marked_seen: AtomicUsize::new(0),
    });
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    shell.install_approval(Arc::new(AllowAll));
    let scout = SubagentDef {
        name: "scout".into(),
        description: "looks around".into(),
        model: None,
        max_steps: None,
        body: "You are the scout. Look.".into(),
    };
    let tools = Arc::new(
        SubagentTools::new(provider.clone(), vec![worker_def(), scout], shell, "m")
            .with_max_depth(2),
    );
    let result = tools
        .resolve(call("worker", "delegate the look"), &ToolContext::default())
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "outer done");
    // The nested level ran once, the worker saw its report and ended:
    // two marked rounds, then stop.
    assert_eq!(provider.marked_seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn at_zero_budget_nothing_is_advertised() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = SubagentTools::new(provider, vec![worker_def()], shell, "m").with_max_depth(0);
    assert!(tools.specs().is_empty());
}

#[test]
fn empty_definitions_advertise_nothing() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = SubagentTools::new(provider, Vec::new(), shell, "m");
    assert!(tools.specs().is_empty());
}

#[test]
fn engine_errors_surface_in_band() {
    // A definition whose body is a broken template fails its render,
    // which comes back as the result's error text.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let provider = Scripted::new(vec![end_turn("never")]);
        let mut def = worker_def();
        def.body = "{% for %}".into();
        let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
        let tools = Arc::new(SubagentTools::new(provider, vec![def], shell, "m"));
        let result = tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("template"), "{}", result.content);
    });
}

#[test]
fn the_inner_requests_carry_the_configured_thinking() {
    // The parent turn's posture rides into the inner agent: the caller
    // sets it, the default is the provider's own decision.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let provider = Scripted::new(vec![end_turn("done")]);
        let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
        let tools = Arc::new(
            SubagentTools::new(provider.clone(), vec![worker_def()], shell, "m")
                .with_thinking(ThinkingEffort::Low),
        );
        let result = tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            provider.thinkings_seen(),
            vec![ThinkingEffort::Low],
            "the configured effort reaches the inner request"
        );
    });
}

#[test]
fn without_a_set_thinking_the_inner_requests_stay_default() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let provider = Scripted::new(vec![end_turn("done")]);
        let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
        let tools = Arc::new(SubagentTools::new(
            provider.clone(),
            vec![worker_def()],
            shell,
            "m",
        ));
        let _ = tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await;
        assert_eq!(
            provider.thinkings_seen(),
            vec![ThinkingEffort::Default],
            "no caller choice means the provider default"
        );
    });
}
