//! The `subagent` tool end-to-end over a scripted provider: the start
//! and fetch shapes, the registry lifecycle (running → finished →
//! fetch → cached; failure → error result; unknown id; queueing past
//! the cap), approval inheritance held past the turn, nesting, and the
//! escape semantics — the parent turn's cancel flag never reaches a
//! job.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rhema_agent_tools::{AllowAll, Approval, ShellTools};
use rhema_core::request::ThinkingEffort;
use rhema_core::{
    Content, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError, Role,
    StopReason, ToolCall, ToolContext, ToolResolver, ToolResult, Usage,
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
/// request's tool names, message count, and every `Tool`-role result
/// that reached the model, for the assertions below.
struct Scripted {
    replies: Vec<ModelOutput>,
    served: AtomicUsize,
    tool_names: Mutex<Vec<Vec<String>>>,
    tool_results: Mutex<Vec<String>>,
    thinking: Mutex<Vec<ThinkingEffort>>,
    message_counts: Mutex<Vec<usize>>,
}

impl Scripted {
    fn new(replies: Vec<ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            replies,
            served: AtomicUsize::new(0),
            tool_names: Mutex::new(Vec::new()),
            tool_results: Mutex::new(Vec::new()),
            thinking: Mutex::new(Vec::new()),
            message_counts: Mutex::new(Vec::new()),
        })
    }

    fn served_tools(&self) -> Vec<Vec<String>> {
        self.tool_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn served_calls(&self) -> usize {
        self.served.load(Ordering::SeqCst)
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

    /// The history length every request carried, in order.
    fn message_counts(&self) -> Vec<usize> {
        self.message_counts
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
        self.message_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req.messages.len());
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

/// Never answers: a job parked in its model stream, exactly the shape a
/// fetch of a running job meets.
struct Hanging;

#[async_trait]
impl ModelProvider for Hanging {
    async fn complete(
        &self,
        _req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        std::future::pending().await
    }
}

/// Fails its first `fails` calls with a dead-link error, then answers
/// every call with the report. The shape an outage has to one job: a
/// stretch of failures, then recovery.
struct Flaky {
    fails: AtomicUsize,
    calls: AtomicUsize,
}

impl Flaky {
    fn new(fails: usize) -> Arc<Self> {
        Arc::new(Self {
            fails: AtomicUsize::new(fails),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ModelProvider for Flaky {
    async fn complete(
        &self,
        _req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let left = self.fails.load(Ordering::SeqCst);
        if left > 0 {
            self.fails.store(left - 1, Ordering::SeqCst);
            return Err(ProviderError::Network(
                "error decoding response body".into(),
            ));
        }
        Ok(end_turn("the report"))
    }
}

/// Answers its first call only after [`Self::release`] fires, then
/// answers every call with a distinct text. Drives the cap test: the
/// first job holds the slot until released, the second queues.
struct Gated {
    calls: AtomicUsize,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ModelProvider for Gated {
    async fn complete(
        &self,
        _req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.release.notified().await;
        }
        Ok(end_turn(if call == 0 {
            "first report"
        } else {
            "second report"
        }))
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
fn worker_tools(provider: Arc<dyn ModelProvider>) -> (Arc<ShellTools>, Arc<SubagentTools>) {
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = Arc::new(SubagentTools::new(
        provider,
        vec![worker_def()],
        shell.clone(),
        "parent-model",
    ));
    (shell, tools)
}

fn call(name: &str, prompt: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({ "name": name, "prompt": prompt }),
        ..ToolCall::default()
    }
}

/// The job id a start result names.
fn job_id(result: &ToolResult) -> String {
    assert!(!result.is_error, "{}", result.content);
    let start = result.content.find("job-").expect("a job id in the result");
    let rest = &result.content[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// Waits until `id` finished: a finished job leaves the status
/// snapshot at once — it carries working jobs only — and its finish
/// event names the outcome (ok or error — the fetch tells which).
async fn wait_finished(handle: &rhema_subagents::SubagentHandle, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle.status().iter().all(|s| s.id != id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} never finished: {:?}",
            handle.status()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_start_returns_at_once_and_the_fetch_returns_the_report() {
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
    let handle = tools.handle();

    let result = tools
        .resolve(call("worker", "do it"), &ToolContext::default())
        .await;
    let id = job_id(&result);
    // The start named the job and the notification to come; the report
    // was not in it.
    assert!(
        result.content.contains("notification"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("the report"));
    // The inner turn rides no output channel: the subagent is its own
    // line on the parent's screen, its report the fetch.
    let status = handle.status();
    assert_eq!(status.len(), 1, "{status:?}");

    wait_finished(&handle, &id).await;
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(!report.is_error, "{}", report.content);
    assert_eq!(report.content, "the report");

    // The job ran in the background with the tools it inherits, and the
    // inner model saw its command's output.
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
        "{:?}",
        provider.results_seen()
    );
    // The hook was captured at spawn, before the fetch.
    assert_eq!(spy.taken(), vec!["bash".to_owned()]);
    // The fetched job left the status snapshot.
    assert!(handle.status().is_empty());
}

fn fetch(id: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        args: serde_json::json!({ "id": id }),
        ..ToolCall::default()
    }
}

#[tokio::test]
async fn a_double_fetch_returns_the_cache_never_a_rerun() {
    let provider = Scripted::new(vec![end_turn("the report")]);
    let (_shell, tools) = worker_tools(provider.clone());
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let calls_after_run = provider.served_calls();

    let first = tools.resolve(fetch(&id), &ToolContext::default()).await;
    let second = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert_eq!(first.content, "the report");
    assert_eq!(second.content, "the report");
    assert_eq!(
        provider.served_calls(),
        calls_after_run,
        "the fetch is a read of the registry, not an execution"
    );
}

#[tokio::test]
async fn an_unknown_id_fetches_as_gone() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let (_shell, tools) = worker_tools(provider);
    let result = tools.resolve(fetch("job-9"), &ToolContext::default()).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("no such job") && result.content.contains("gone"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_fetch_of_a_running_job_is_still_running() {
    let (_shell, tools) = worker_tools(Arc::new(Hanging));
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(result.is_error);
    assert!(
        result.content.contains(&id) && result.content.contains("still running"),
        "{}",
        result.content
    );
    // The job is on the snapshot, waiting: the UI has its line.
    assert_eq!(handle.status()[0].id, id);
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

    // The two shapes are exclusive: both present is an error, and so
    // is neither.
    let both = tools
        .resolve(
            ToolCall {
                id: "c3".into(),
                name: "subagent".into(),
                args: serde_json::json!({ "name": "worker", "prompt": "p", "id": "job-1" }),
                ..ToolCall::default()
            },
            &ctx,
        )
        .await;
    assert!(
        both.is_error && both.content.contains("not both"),
        "{}",
        both.content
    );

    let neither = tools
        .resolve(
            ToolCall {
                id: "c4".into(),
                name: "subagent".into(),
                args: serde_json::json!({}),
                ..ToolCall::default()
            },
            &ctx,
        )
        .await;
    assert!(
        neither.is_error && neither.content.contains("`id`"),
        "{}",
        neither.content
    );

    let blank_id = tools
        .resolve(
            ToolCall {
                id: "c5".into(),
                name: "subagent".into(),
                args: serde_json::json!({ "id": "  " }),
                ..ToolCall::default()
            },
            &ctx,
        )
        .await;
    assert!(blank_id.is_error && blank_id.content.contains("empty"));
}

#[tokio::test]
async fn the_captured_hook_outlives_the_turn() {
    let provider = Scripted::new(vec![
        tool_use("i1", "bash", serde_json::json!({"command": "echo later"})),
        end_turn("the report"),
    ]);
    let (shell, tools) = worker_tools(provider.clone());
    let spy = Spy::recorder(true);
    let handle = tools.handle();

    shell.install_approval(spy.clone());
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    // The turn ends — the driver clears the hook. The job holds the
    // hook it captured at spawn, so its gated command is answered by
    // the turn's policy, never the fail-safe reject.
    shell.clear_approval();
    assert!(shell.current_approval().is_none());

    wait_finished(&handle, &id).await;
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert_eq!(report.content, "the report");
    assert_eq!(spy.taken(), vec!["bash".to_owned()]);
    assert!(
        provider
            .results_seen()
            .iter()
            .any(|content| content.contains("later")),
        "{:?}",
        provider.results_seen()
    );
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
    let handle = tools.handle();

    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
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
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert_eq!(report.content, "blocked");
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
async fn an_exhausted_budget_fetches_as_its_error() {
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
    let handle = tools.handle();

    let id = job_id(
        &tools
            .resolve(call("worker", "loop forever"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    // The failure announced itself, error reason and all.
    assert_eq!(
        handle.steering().drain(),
        vec![
            "Subagent worker (job-1) has finished with an error: hit its step budget of 1."
                .to_owned()
        ]
    );
    let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("subagent worker") && result.content.contains("step budget of 1"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn the_parent_turns_cancel_flag_never_reaches_a_job() {
    let provider = Scripted::new(vec![end_turn("done despite escape")]);
    let (_shell, tools) = worker_tools(provider);
    let handle = tools.handle();
    let ctx = ToolContext {
        cancelled: Arc::new(AtomicBool::new(true)),
        ..ToolContext::default()
    };
    // Escape flipped the main turn's flag; the start still works and
    // the job runs to its report on its own flag.
    let id = job_id(&tools.resolve(call("worker", "do it"), &ctx).await);
    wait_finished(&handle, &id).await;
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert_eq!(report.content, "done despite escape");
}

#[tokio::test]
async fn starts_past_the_cap_queue_until_a_slot_frees() {
    let gated = Arc::new(Gated {
        calls: AtomicUsize::new(0),
        release: tokio::sync::Notify::new(),
    });
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    shell.install_approval(Arc::new(AllowAll));
    let tools = Arc::new(
        SubagentTools::new(gated.clone(), vec![worker_def()], shell, "m").with_max_jobs(1),
    );
    let handle = tools.handle();
    let ctx = ToolContext::default();

    let first = job_id(&tools.resolve(call("worker", "first"), &ctx).await);
    // The first job takes the one slot.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle
        .status()
        .iter()
        .any(|s| s.id == first && s.phase != "queued")
    {
        assert!(Instant::now() < deadline, "the first job never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The second queues — id minted and returned at once.
    let second = job_id(&tools.resolve(call("worker", "second"), &ctx).await);
    assert_ne!(first, second);
    let phases: Vec<(String, String)> = handle
        .status()
        .iter()
        .map(|s| (s.id.clone(), s.phase.clone()))
        .collect();
    assert_eq!(
        phases,
        vec![
            (first.clone(), "thinking".to_owned()),
            (second.clone(), "queued".to_owned())
        ],
        "{phases:?}"
    );

    // A slot frees: the queued job begins and finishes.
    gated.release.notify_one();
    wait_finished(&handle, &first).await;
    wait_finished(&handle, &second).await;
    assert_eq!(
        tools.resolve(fetch(&first), &ctx).await.content,
        "first report"
    );
    assert_eq!(
        tools.resolve(fetch(&second), &ctx).await.content,
        "second report"
    );
}

#[tokio::test]
async fn the_notification_names_the_job_once_per_finish() {
    let provider = Scripted::new(vec![end_turn("the report")]);
    let (_shell, tools) = worker_tools(provider);
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    assert!(
        handle.steering().drain().is_empty(),
        "a running job announces nothing"
    );
    wait_finished(&handle, &id).await;
    let expected = format!("Subagent worker ({id}) has finished.");
    // A pure query: the same lines come back until the driver appends
    // them and the model fetches.
    assert_eq!(handle.steering().drain(), vec![expected.clone()]);
    assert_eq!(handle.steering().drain(), vec![expected]);
    // The fetch retires the announcement with the UI line.
    let _ = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(handle.steering().drain().is_empty());
    assert!(handle.status().is_empty());
}

#[tokio::test]
async fn nesting_follows_the_budget() {
    // Budget 1 (the default): the inner agent sees no subagent tool.
    let provider = Scripted::new(vec![end_turn("leaf")]);
    let (_shell, tools) = worker_tools(provider.clone());
    tools.specs();
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    assert_eq!(
        provider.served_tools()[0],
        vec!["bash".to_owned()],
        "no subagent tool inside at the default budget"
    );

    // Budget 2: the inner agent sees one, and a nested call resolves —
    // synchronously, to its report inside the job's step loop.
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
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "delegate the look"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "outer done");
    // The nested level ran once, the worker saw its report and ended:
    // two marked rounds, then stop.
    assert_eq!(provider.marked_seen.load(Ordering::SeqCst), 2);
    assert!(
        handle.status().is_empty(),
        "a nested call registers no job of its own"
    );
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
fn the_start_stays_replay_unsafe() {
    let provider = Scripted::new(vec![end_turn("never")]);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = SubagentTools::new(provider, vec![worker_def()], shell, "m");
    assert_eq!(
        tools.replay_safety("subagent"),
        rhema_core::ReplaySafety::Unsafe,
        "a fetch must not launder the start's classification"
    );
}

#[tokio::test]
async fn engine_errors_surface_in_band() {
    // A provider that fails: the fetch returns the reason as the
    // result's error text — failure is in-band, never a panic, never
    // an abort of the parent's turn.
    struct Failing;

    #[async_trait::async_trait]
    impl ModelProvider for Failing {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Err(ProviderError::Api {
                message: "the engine broke".into(),
                status: Some(500),
            })
        }
    }

    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    // A 500 is retryable, so the background ladder would wait it out;
    // this test is about the surfacing, and the empty ladder fails the
    // first attempt at once.
    let tools = Arc::new(
        SubagentTools::new(Arc::new(Failing), vec![worker_def()], shell, "m")
            .with_retry_ladders(vec![], vec![]),
    );
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("the engine broke"),
        "{}",
        result.content
    );
}

/// The inner requests ride the transcript view (`rhema_transcript::
/// attach` in the inner run): a short run proves the decorator is wired
/// and pass-through — below the transcript's budget the whole inner
/// history still reaches the provider, prompt and tool rounds alike.
#[tokio::test]
async fn a_short_run_reads_through_the_transcript_whole() {
    let provider = Scripted::new(vec![
        tool_use("i1", "bash", serde_json::json!({"command": "echo through"})),
        end_turn("the report"),
    ]);
    let (shell, tools) = worker_tools(provider.clone());
    shell.install_approval(Arc::new(AllowAll));
    let handle = tools.handle();

    let id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(!report.is_error, "{}", report.content);
    // Request 1: the prompt alone. Request 2: the prompt, the
    // assistant's tool call, the tool's result — nothing dropped or
    // rewritten on the way through the bounded view.
    assert_eq!(provider.message_counts(), vec![1, 3]);
    assert!(
        provider
            .results_seen()
            .iter()
            .any(|content| content.contains("through")),
        "{:?}",
        provider.results_seen()
    );
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
        let handle = tools.handle();
        let id = job_id(
            &tools
                .resolve(call("worker", "do it"), &ToolContext::default())
                .await,
        );
        wait_finished(&handle, &id).await;
        let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
        assert!(!report.is_error, "{}", report.content);
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
        let handle = tools.handle();
        let id = job_id(
            &tools
                .resolve(call("worker", "do it"), &ToolContext::default())
                .await,
        );
        wait_finished(&handle, &id).await;
        let _ = tools.resolve(fetch(&id), &ToolContext::default()).await;
        assert_eq!(
            provider.thinkings_seen(),
            vec![ThinkingEffort::Default],
            "no caller choice means the provider default"
        );
    });
}

#[tokio::test]
async fn a_job_dies_with_its_registry() {
    // Dropping the tools drops the registry; a running job ends at its
    // next step boundary instead of running on. The provider counts a
    // second call: it never comes.
    struct Counting {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl ModelProvider for Counting {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if call == 0 {
                tool_use("i1", "bash", serde_json::json!({"command": "sleep 0.5"}))
            } else {
                std::future::pending().await
            })
        }
    }

    let provider = Arc::new(Counting {
        calls: AtomicUsize::new(0),
    });
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    shell.install_approval(Arc::new(AllowAll));
    let mut def = worker_def();
    def.max_steps = Some(5);
    let tools = Arc::new(SubagentTools::new(provider.clone(), vec![def], shell, "m"));
    let handle = tools.handle();
    let _id = job_id(
        &tools
            .resolve(call("worker", "do it"), &ToolContext::default())
            .await,
    );
    // Wait for the job's step to reach its tool call — the shell's
    // `sleep` is mid-run and no second model call has happened — then
    // drop everything.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let running = handle
            .status()
            .first()
            .is_some_and(|s| s.phase == "running");
        assert!(
            Instant::now() < deadline,
            "the job never reached its tool call: {:?}",
            handle.status()
        );
        if running && provider.calls.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(tools);
    drop(handle);
    let seen = provider.calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        seen,
        "the job stopped with its registry"
    );
}

/// A provider that only records the system prompts it is sent, and
/// ends every request at once.
struct Capture {
    systems: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ModelProvider for Capture {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.systems
            .lock()
            .unwrap()
            .push(req.system.unwrap_or_default());
        Ok(end_turn("the report"))
    }
}

/// The stamp a job is handed rides into the inner system prompt, with
/// the definition's body appended verbatim under it — template
/// syntax in the body reaches the model as the author wrote it.
#[tokio::test]
async fn a_job_runs_under_the_supplied_base_prompt() {
    let capture = Arc::new(Capture {
        systems: Mutex::new(Vec::new()),
    });
    let def = SubagentDef {
        name: "worker".into(),
        description: "works".into(),
        model: None,
        max_steps: None,
        body: "THE BODY TEXT with {{ braces }} that stay.".into(),
    };
    let tools = Arc::new(
        SubagentTools::new(
            capture.clone(),
            vec![def],
            Arc::new(ShellTools::new(std::env::temp_dir())),
            "parent-model",
        )
        .with_base_system("THE STAMPED BASE".into()),
    );
    let handle = tools.handle();

    let result = tools
        .resolve(call("worker", "go"), &ToolContext::default())
        .await;
    wait_finished(&handle, &job_id(&result)).await;

    let systems = capture.systems.lock().unwrap();
    assert_eq!(
        systems.as_slice(),
        ["THE STAMPED BASE\n\nTHE BODY TEXT with {{ braces }} that stay."],
        "{systems:?}"
    );
}

#[tokio::test]
async fn a_dead_link_inside_a_job_retries_and_completes() {
    // The job-tuned wrapper: the frontend's turn ladder never appears
    // here — the provider is raw — so this retry is the subagents'
    // own, and one failure plus a one-tick ladder still ends in a
    // report, not a failed job.
    let provider = Flaky::new(1);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = Arc::new(
        SubagentTools::new(provider.clone(), vec![worker_def()], shell, "parent-model")
            .with_retry_ladders(vec![Duration::from_millis(1)], vec![]),
    );
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "go"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;

    let events = handle.take_finished();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].line, "Subagent worker (job-1) has finished.");
    assert_eq!(provider.calls(), 2, "one retry spent on the dead link");
    let report = tools.resolve(fetch(&id), &ToolContext::default()).await;
    assert!(!report.is_error, "{}", report.content);
    assert_eq!(report.content, "the report");
}

#[tokio::test]
async fn an_outage_that_outruns_the_ladder_fails_transiently_and_retry_restarts_it() {
    // No ladder at all: the first attempt dies, the job fails with a
    // transient error — the class the retry command picks up. The
    // restart re-runs the same id from its start, and the recovered
    // network answers.
    let provider = Flaky::new(1);
    let shell = Arc::new(ShellTools::new(std::env::temp_dir()));
    let tools = Arc::new(
        SubagentTools::new(provider.clone(), vec![worker_def()], shell, "parent-model")
            .with_retry_ladders(vec![], vec![]),
    );
    let handle = tools.handle();
    let id = job_id(
        &tools
            .resolve(call("worker", "go"), &ToolContext::default())
            .await,
    );
    wait_finished(&handle, &id).await;
    assert_eq!(
        handle.take_finished()[0].line,
        "Subagent worker (job-1) has finished with an error: \
         network error: error decoding response body."
    );

    // The restart keeps the id and re-runs from the start.
    let restarted = handle.restart_failed_jobs();
    assert_eq!(
        restarted,
        vec![rhema_subagents::Restarted {
            id: id.clone(),
            name: "worker".into(),
        }]
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let report = loop {
        let result = tools.resolve(fetch(&id), &ToolContext::default()).await;
        if !result.is_error {
            break result;
        }
        // A still-queued or running job fetches as "still running".
        assert!(
            result.content.contains("still running"),
            "the failed cache must have cleared: {}",
            result.content
        );
        assert!(
            Instant::now() < deadline,
            "the restart never finished: {}",
            result.content
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(report.content, "the report");

    // The re-run announces itself into the id the model already knows.
    let events = handle.take_finished();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].line, "Subagent worker (job-1) has finished.");
    assert_eq!(provider.calls(), 2, "one attempt per run");
}
