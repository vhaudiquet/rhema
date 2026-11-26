//! The step engine: [`Agent::step`], one exchange, never a loop.
//!
//! `step()` semantics are exact (SPEC.md §4): emit `StepStart`, hand the
//! session to the commit hook (the stream's input durable), assemble the
//! request from the session, stream through the provider, append the
//! assistant message, then, on `ToolUse`, hand the session to the commit
//! hook again (commit-before-execute), resolve each call in order, append
//! its `Tool`-role result, hand the session to the hook a third time (the
//! stream's outcome durable), and emit `StepEnd`. Tool errors never abort;
//! they are in-band `is_error` results. [`Agent::turn`] and [`run_turn`]
//! are the provided loop over steps; the step boundary stays the
//! primitive a driver can persist, cancel, or steer around.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::event::{Event, EventSink};
use crate::message::{Content, Message, Role};
use crate::output::{ModelOutput, Step, StopReason};
use crate::provider::{ModelProvider, ProviderError};
use crate::request::{ChatParams, ModelRequest};
use crate::session::Session;
use crate::tool::{
    ReplaySafety, ToolCall, ToolContext, ToolProgress, ToolResolver, ToolResult, ToolStatus,
};

/// Why a step or a turn failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// The agent was built without a [`ModelProvider`].
    #[error("agent built without a model provider")]
    MissingProvider,
    /// The model call failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The commit hook rejected a checkpoint (see [`CommitHook`]): at a
    /// step boundary, or before a tool ran. The step fails so the driver
    /// surfaces the persistence problem before tokens are spent or a side
    /// effect starts.
    #[error(transparent)]
    Commit(#[from] CommitError),
}

/// Why a [`CommitHook`] rejected the intent: the text a driver's
/// persistence layer reported, carried as a typed error so the hook's
/// failure keeps its own type through [`AgentError::Commit`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("commit failed: {0}")]
pub struct CommitError(
    /// What went wrong, worded for the driver's user.
    pub String,
);

impl AgentError {
    /// Whether a retry could clear this failure: a model call that
    /// failed with a retryable provider error, a dead link, a
    /// transport timeout, a struggling server (see
    /// [`ProviderError::is_retryable`]). Everything else, a rejected
    /// key, an exhausted budget, a commit hook that would fail again,
    /// repeats identically, and a caller that retries only burns the
    /// attempt. This is the question a background runner asks when a
    /// step dies: does the failure name the infrastructure, which may
    /// come back, or the work itself, which will not.
    pub fn is_retryable(&self) -> bool {
        matches!(self, AgentError::Provider(err) if err.is_retryable())
    }
}

/// Forwards every event to the caller's sink while recording which tool
/// calls the provider already announced, so the engine does not echo a
/// streamed `ToolCallStart`.
struct ForwardingSink<'a> {
    inner: &'a mut dyn EventSink,
    announced: HashSet<String>,
}

impl EventSink for ForwardingSink<'_> {
    fn emit(&mut self, event: Event<'_>) {
        if let Event::ToolCallStart { id, .. } = event {
            self.announced.insert(id.to_owned());
        }
        self.inner.emit(event);
    }
}

/// Stamps a running call's id onto a resolver's live reports: output
/// chunks as ephemeral `ToolOutputDelta`s, status as ephemeral
/// `ToolStatus`es. The resolver knows neither the id nor the sink.
struct CallProgress<'sink, 'a> {
    id: String,
    events: &'sink mut ForwardingSink<'a>,
}

impl ToolProgress for CallProgress<'_, '_> {
    fn output(&mut self, chunk: &str) {
        self.events.emit(Event::ToolOutputDelta {
            id: &self.id,
            chunk,
        });
    }

    fn status(&mut self, status: ToolStatus) {
        self.events.emit(Event::ToolStatus {
            id: &self.id,
            status: &status,
        });
    }
}

/// The checkpoint seam: called by [`Agent::step`] at every stream boundary,
/// so a session that dies mid-turn is always resumable at the last one.
/// Three fires per tool step, two per plain step:
///
/// 1. at the step's start, before the request, the caller's new input is
///    durable before tokens are spent on it;
/// 2. on a `ToolUse` stop, after the assistant message is appended and
///    before the first call resolves, commit-before-execute. Persisting
///    here is what makes a crash inside a tool call resumable without
///    losing the fact that the call was decided: the durable history ends
///    with the committed intent, and the resume path
///    ([`crate::ToolResolver::replay_safety`]) decides between re-running
///    a safe call and reporting an unsafe one interrupted;
/// 3. at the step's end, after the results (or after the assistant message
///    when no tools follow), the stream's outcome is durable before the
///    driver hears of the step's completion.
///
/// An `Err` fails the step at the boundary it fired on, the engine never
/// spends tokens on, or starts a side effect for, a session it could not
/// commit.
pub type CommitHook = Box<dyn Fn(&Session) -> Result<(), CommitError> + Send + Sync>;

/// The step engine: a provider, an optional tool resolver, and the fixed
/// prompt/params it stamps into every request.
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    resolver: Option<Arc<dyn ToolResolver>>,
    system: Option<String>,
    params: ChatParams,
    commit: Option<CommitHook>,
    steps: u32,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The seams hold dyn traits: they print as placeholders, the
        // plain data prints in full.
        f.debug_struct("Agent")
            .field("provider", &"<dyn ModelProvider>")
            .field(
                "resolver",
                &self.resolver.as_ref().map(|_| "<dyn ToolResolver>"),
            )
            .field("system", &self.system)
            .field("params", &self.params)
            .field("commit", &self.commit.as_ref().map(|_| "<commit hook>"))
            .field("steps", &self.steps)
            .finish()
    }
}

impl Agent {
    /// Start configuring an [`Agent`].
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// ONE exchange. Never loops. Appends the assistant message and any tool
    /// results to `session.history` in place, so the next step sees them.
    pub async fn step(
        &mut self,
        session: &mut Session,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
    ) -> Result<Step, AgentError> {
        let iteration = self.steps;
        self.steps += 1;

        // An interrupted predecessor, a cancelled turn dropped
        // mid-await, a failed commit between the call's durable intent
        // and its result, leaves dangling calls in the history, and
        // the next request would be invalid carrying them. The repair
        // rides this step's first commit like any result would.
        close_orphaned_calls(&mut session.history);

        let mut events = ForwardingSink {
            inner: sink,
            announced: HashSet::new(),
        };
        events.emit(Event::StepStart { iteration });

        // Per-stream durability: the session as it stands, the caller's
        // new input included, is committed before the request spends
        // tokens on it. A death mid-stream leaves a file the next start
        // resumes from, instead of a turn that never happened.
        if let Some(commit) = &self.commit {
            commit(session).map_err(AgentError::Commit)?;
        }

        let req = ModelRequest {
            system: self.system.clone(),
            messages: session.history.clone(),
            tools: self
                .resolver
                .as_ref()
                .map(|r| r.specs())
                .unwrap_or_default(),
            params: self.params.clone(),
        }
        .normalized();
        let out = self.provider.complete(req, &mut events).await?;

        let call_indices: Vec<usize> = out
            .message
            .content
            .iter()
            .enumerate()
            .filter_map(|(i, c)| matches!(c, Content::ToolCall(_)).then_some(i))
            .collect();
        session.history.push(out.message.clone());

        // A `ToolUse` stop with no assembled call has nothing to run.
        // Stepping again would spend one request per iteration on an
        // answer the provider never finished, and the next loop turn
        // would ask the model to answer calls that are not there.
        if out.stop_reason == StopReason::ToolUse && !call_indices.is_empty() {
            if let Some(commit) = &self.commit {
                // Commit-before-execute: the intent is durable before the
                // first side effect, or nothing runs.
                commit(session).map_err(AgentError::Commit)?;
            }
            for i in call_indices {
                let Some(Content::ToolCall(call)) = out.message.content.get(i) else {
                    continue;
                };
                if !events.announced.contains(&call.id) {
                    // A provider that did not stream the call start still
                    // owes the sink one lifecycle event before the result.
                    events.emit(Event::ToolCallStart {
                        id: &call.id,
                        name: &call.name,
                    });
                }
                // The call's live output rides the sink as ephemeral
                // deltas, the call's id stamped on by the engine, the
                // resolver knows only its own progress lines; its status
                // rides the same stamp. The stamp's borrow ends with the
                // call, before the result event.
                // A cancelled turn stops between calls, not only inside
                // one: the resolver polls the flag for a call in flight,
                // this covers the calls queued behind it, and each still
                // gets a result so the next request stays valid.
                let cancelled = ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed);
                let result = if cancelled {
                    ToolResult::failure(&call.id, "cancelled before the call ran")
                } else {
                    let mut progress = CallProgress {
                        id: call.id.clone(),
                        events: &mut events,
                    };
                    match &self.resolver {
                        Some(resolver) => {
                            resolver
                                .resolve_streaming(call.clone(), ctx, &mut progress)
                                .await
                        }
                        None => ToolResult::failure(&call.id, "no tool resolver is configured"),
                    }
                };
                events.emit(Event::ToolResult {
                    id: &result.id,
                    content: &result.content,
                    is_error: result.is_error,
                    code: result.code,
                });
                session.history.push(Message {
                    annotations: Default::default(),
                    role: Role::Tool,
                    content: vec![Content::ToolResult(result)],
                });
            }
        }

        // The stream's outcome, results included, when tools ran, is
        // durable before the step is announced complete. A plain step
        // lands here too, so a final answer that never touches a tool is
        // still checkpointed.
        if let Some(commit) = &self.commit {
            commit(session).map_err(AgentError::Commit)?;
        }

        events.emit(Event::StepEnd {
            usage: out.usage,
            stop_reason: &out.stop_reason,
        });
        Ok(Step { output: out })
    }

    /// The default iteration cap of [`Agent::turn`]: a runaway-loop
    /// guard, not a task budget. A driver with its own policy passes a
    /// different cap to [`Agent::turn_with_limit`]. The cap is
    /// non-zero because a turn always runs at least one step.
    pub const DEFAULT_TURN_STEPS: NonZeroU32 = NonZeroU32::new(32).unwrap();

    /// A whole turn: append `input`, then step until the model answers
    /// without asking for tools, up to [`Self::DEFAULT_TURN_STEPS`].
    ///
    /// `input` is a user message; a `&str` or `String` is accepted, and
    /// so is a [`Message`] carrying images or a different role. The loop
    /// is the common case; [`Agent::step`] remains the primitive, one
    /// exchange that stops even when the model asked for tools.
    pub async fn turn(
        &mut self,
        input: impl Into<Message>,
        session: &mut Session,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
    ) -> Result<ModelOutput, AgentError> {
        self.turn_with_limit(input, session, sink, ctx, Self::DEFAULT_TURN_STEPS)
            .await
    }

    /// [`Agent::turn`] with the loop guard chosen by the caller.
    pub async fn turn_with_limit(
        &mut self,
        input: impl Into<Message>,
        session: &mut Session,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
        max_iterations: NonZeroU32,
    ) -> Result<ModelOutput, AgentError> {
        run_turn(self, session, input.into(), sink, ctx, max_iterations).await
    }

    /// Continue a turn whose input is already in the history: step until
    /// the model answers without asking for tools, appending nothing
    /// first. The answer to a turn whose input was committed before a
    /// crash, or the continuation of one that crashed mid tool loop.
    ///
    /// [`Agent::turn`] plus this pair cover a resumed turn: `turn` pushes
    /// the input and loops, `resume_turn` loops over the tail the dead
    /// process left. Calling `turn` again instead would append the same
    /// user message twice. Dangling calls in the tail are closed by the
    /// first step's repair, as in any step.
    pub async fn resume_turn(
        &mut self,
        session: &mut Session,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
    ) -> Result<ModelOutput, AgentError> {
        self.resume_turn_with_limit(session, sink, ctx, Self::DEFAULT_TURN_STEPS)
            .await
    }

    /// [`Agent::resume_turn`] with the loop guard chosen by the caller.
    pub async fn resume_turn_with_limit(
        &mut self,
        session: &mut Session,
        sink: &mut dyn EventSink,
        ctx: &ToolContext,
        max_iterations: NonZeroU32,
    ) -> Result<ModelOutput, AgentError> {
        let mut step = self.step(session, sink, ctx).await?;
        let mut remaining = max_iterations.get() - 1;
        while step.output.stop_reason == StopReason::ToolUse && remaining > 0 {
            step = self.step(session, sink, ctx).await?;
            remaining -= 1;
        }
        Ok(step.output)
    }

    /// The system prompt stamped into every request, as set by
    /// [`AgentBuilder::system`]; `None` when none was set.
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// Replaces the commit hook after building; `None` disables it. See
    /// [`CommitHook`].
    pub fn set_commit_hook(&mut self, hook: Option<CommitHook>) {
        self.commit = hook;
    }
}

/// Builder for [`Agent`]; the provider is required, everything else
/// defaults.
#[derive(Default)]
pub struct AgentBuilder {
    provider: Option<Arc<dyn ModelProvider>>,
    resolver: Option<Arc<dyn ToolResolver>>,
    system: Option<String>,
    model: Option<String>,
    params: Option<ChatParams>,
    commit: Option<CommitHook>,
}

impl std::fmt::Debug for AgentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The seams hold dyn traits: they print as placeholders, the
        // plain data prints in full.
        f.debug_struct("AgentBuilder")
            .field(
                "provider",
                &self.provider.as_ref().map(|_| "<dyn ModelProvider>"),
            )
            .field(
                "resolver",
                &self.resolver.as_ref().map(|_| "<dyn ToolResolver>"),
            )
            .field("system", &self.system)
            .field("model", &self.model)
            .field("params", &self.params)
            .field("commit", &self.commit.as_ref().map(|_| "<commit hook>"))
            .finish()
    }
}

impl AgentBuilder {
    /// Sets the model backend (required). A concrete provider and an
    /// `Arc` the caller already holds both pass unchanged: `Arc<T>` is a
    /// `ModelProvider` when `T` is.
    pub fn provider(mut self, provider: impl ModelProvider + 'static) -> Self {
        self.provider = Some(Arc::new(provider));
        self
    }

    /// Sets the tool resolver; without one, `specs()` is empty and a `ToolUse`
    /// stop resolves to an in-band error result. A concrete resolver and
    /// an `Arc` both pass, as with [`AgentBuilder::provider`].
    pub fn resolver(mut self, resolver: impl ToolResolver + 'static) -> Self {
        self.resolver = Some(Arc::new(resolver));
        self
    }

    /// Sets the model identifier; shorthand for the `model` field of
    /// [`params`](AgentBuilder::params), which still carries the rest.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the system prompt stamped into every request.
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the sampling parameters stamped into every request.
    pub fn params(mut self, params: ChatParams) -> Self {
        self.params = Some(params);
        self
    }

    /// Sets the commit-before-execute hook (see [`CommitHook`]). Without
    /// one, a `ToolUse` step relies on the driver's post-round checkpoints
    /// alone, and a crash inside a tool call loses the record that the call
    /// was decided.
    pub fn commit_hook(mut self, hook: CommitHook) -> Self {
        self.commit = Some(hook);
        self
    }

    /// Builds the agent; fails if no provider was set.
    pub fn build(self) -> Result<Agent, AgentError> {
        let mut params = self.params.unwrap_or_default();
        if let Some(model) = self.model {
            params.model = model;
        }
        Ok(Agent {
            provider: self.provider.ok_or(AgentError::MissingProvider)?,
            resolver: self.resolver,
            system: self.system,
            params,
            commit: self.commit,
            steps: 0,
        })
    }
}

/// Closes tool calls left dangling by an interrupted turn: an
/// assistant message carrying tool calls must be answered by a result
/// for every call before the next request is valid. A cancel (the
/// turn future dropped mid-await, no code of the turn's runs on that
/// path), a failed commit between the call's durable intent and its
/// result, or a crash leaves exactly that shape. The repair inserts
/// one synthetic failure per unanswered call directly after the
/// assistant message that issued it, in call order; returns the call
/// ids it answered.
pub fn close_orphaned_calls(history: &mut Vec<Message>) -> Vec<String> {
    let results = dangling_calls(history).into_iter().map(|call| {
        ToolResult::failure(
            &call.id,
            "interrupted: the turn ended before this call's result landed; \
             its effect may have partially applied, verify state before \
             retrying",
        )
    });
    insert_missing(history, results)
}

/// The unanswered tool calls in `history`, in the order a close answers
/// them: assistant messages in history order, calls in the order the
/// assistant issued them.
fn dangling_calls(history: &[Message]) -> Vec<ToolCall> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < history.len() {
        if history[index].role != Role::Assistant {
            index += 1;
            continue;
        }
        let mut calls = Vec::new();
        let mut seen = HashSet::new();
        for content in &history[index].content {
            if let Content::ToolCall(call) = content
                && seen.insert(call.id.clone())
            {
                calls.push(call.clone());
            }
        }
        if calls.is_empty() {
            index += 1;
            continue;
        }
        // A call is answered by the Tool block that follows its own
        // assistant message, not by any result anywhere in the
        // history: providers reuse ids across turns (`call_0` is
        // routine), so a same-id result from an earlier turn must not
        // mask a dangling call in this one.
        let mut block_end = index + 1;
        while block_end < history.len() && history[block_end].role == Role::Tool {
            block_end += 1;
        }
        let answered: HashSet<&str> = history[index + 1..block_end]
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                Content::ToolResult(result) => Some(result.id.as_str()),
                _ => None,
            })
            .collect();
        found.extend(
            calls
                .into_iter()
                .filter(|call| !answered.contains(call.id.as_str())),
        );
        index = block_end;
    }
    found
}

/// Inserts one result per dangling call, each directly after the
/// assistant message that issued it and in call order, pulling the
/// results in [`dangling_calls`]' order. Returns the call ids answered.
fn insert_missing(
    history: &mut Vec<Message>,
    mut results: impl Iterator<Item = ToolResult>,
) -> Vec<String> {
    let mut repaired = Vec::new();
    let mut index = 0;
    while index < history.len() {
        if history[index].role != Role::Assistant {
            index += 1;
            continue;
        }
        let mut calls = Vec::new();
        let mut seen = HashSet::new();
        for content in &history[index].content {
            if let Content::ToolCall(call) = content
                && seen.insert(call.id.clone())
            {
                calls.push(call.id.clone());
            }
        }
        if calls.is_empty() {
            index += 1;
            continue;
        }
        let mut block_end = index + 1;
        while block_end < history.len() && history[block_end].role == Role::Tool {
            block_end += 1;
        }
        let answered: HashSet<&str> = history[index + 1..block_end]
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                Content::ToolResult(result) => Some(result.id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<&String> = calls
            .iter()
            .filter(|id| !answered.contains(id.as_str()))
            .collect();
        if missing.is_empty() {
            index += 1;
            continue;
        }
        let rank: HashMap<&str, usize> = calls
            .iter()
            .enumerate()
            .map(|(position, id)| (id.as_str(), position))
            .collect();
        for id in missing {
            // The two scans agree today; a caller that hands fewer
            // results than there are dangling calls still gets one per
            // call, named, instead of a panic in the middle of a repair.
            let result = results.next().unwrap_or_else(|| {
                ToolResult::failure(id, "the interrupted call was not repaired")
            });
            let call_rank = rank[id.as_str()];
            let mut insert_at = index + 1;
            for (offset, message) in history[index + 1..block_end].iter().enumerate() {
                let position = index + 1 + offset;
                let mut earlier = false;
                let mut later = false;
                for content in &message.content {
                    if let Content::ToolResult(result) = content
                        && let Some(other) = rank.get(result.id.as_str())
                    {
                        earlier |= *other < call_rank;
                        later |= *other > call_rank;
                    }
                }
                if earlier {
                    insert_at = position + 1;
                }
                if later {
                    break;
                }
            }
            history.insert(
                insert_at,
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(result)],
                    annotations: Default::default(),
                },
            );
            block_end += 1;
            repaired.push(id.clone());
        }
        index = block_end;
    }
    repaired
}

/// Closes calls a crash left committed-but-unanswered, with replay
/// awareness: a call whose tool declared [`ReplaySafety::Safe`] is
/// re-executed now against `resolver` so the round completes; an unsafe
/// one gets an in-band failure telling the model the call was interrupted
/// before its result landed, so it can verify state before retrying. One
/// `Tool` result per dangling call, in call order; a clean history is
/// untouched. A cancelled turn wants [`close_orphaned_calls`] instead: a
/// cancelled call never re-runs, whatever its declared safety.
pub async fn revive_dangling_calls(
    session: &mut Session,
    resolver: &dyn ToolResolver,
    ctx: &ToolContext,
) {
    let dangling = dangling_calls(&session.history);
    if dangling.is_empty() {
        return;
    }
    let mut results = Vec::with_capacity(dangling.len());
    for call in dangling {
        results.push(match resolver.replay_safety(&call.name) {
            ReplaySafety::Safe => resolver.resolve(call, ctx).await,
            ReplaySafety::Unsafe => ToolResult::failure(
                &call.id,
                "interrupted: the call was committed but its result never \
                 landed; its effect may have partially applied, verify \
                 state before retrying",
            ),
        });
    }
    insert_missing(&mut session.history, results.into_iter());
}

/// The loop in free-function form, for a caller that already holds a
/// [`Message`]: push the input, then step until the model stops
/// requesting tools or the cap runs out. [`Agent::turn`] is the
/// shorthand; the step boundary stays the primitive, and a driver with
/// its own persistence or steering policy can own the loop instead.
///
/// The returned output is the last exchange either way. A
/// [`StopReason::ToolUse`] result means the cap ended the turn while the
/// model still wanted tools; its calls already ran and the history is
/// valid, so the caller can continue with [`Agent::step`] or a follow-up
/// turn. A finished turn returns the model's own stop reason.
pub async fn run_turn(
    agent: &mut Agent,
    session: &mut Session,
    input: Message,
    sink: &mut dyn EventSink,
    ctx: &ToolContext,
    max_iterations: NonZeroU32,
) -> Result<ModelOutput, AgentError> {
    session.history.push(input);
    let mut step = agent.step(session, sink, ctx).await?;
    let mut remaining = max_iterations.get() - 1;
    // `ran_tools` keeps a provider that stopped on `ToolUse` without
    // assembling a call from spending every remaining iteration on it.
    while step.output.stop_reason == StopReason::ToolUse && step.ran_tools() && remaining > 0 {
        step = agent.step(session, sink, ctx).await?;
        remaining -= 1;
    }
    Ok(step.output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCall;

    #[test]
    fn a_dangling_call_is_answered_with_one_synthetic_failure() {
        let mut history = vec![
            Message::text(Role::User, "run it"),
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command": "echo hi"}),
                    provider_data: serde_json::Value::Null,
                })],
            },
        ];
        let answered = close_orphaned_calls(&mut history);
        assert_eq!(answered, ["call-1"]);
        assert_eq!(history.len(), 3);
        let Some(Content::ToolResult(result)) = history[2].content.first() else {
            panic!("the tail is the synthetic result");
        };
        assert_eq!(result.id, "call-1");
        assert!(result.is_error, "an interrupted call reads as a failure");
    }

    #[test]
    fn a_reused_call_id_is_answered_per_turn() {
        // Google-style providers restart ids every turn, so the first
        // turn's `call_0` result must not mask the second turn's
        // dangling `call_0`.
        let assistant = |id: &str| Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
                provider_data: serde_json::Value::Null,
            })],
        };
        let mut history = vec![
            Message::text(Role::User, "first"),
            assistant("call_0"),
            Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::failure(
                    "call_0",
                    "already answered",
                ))],
            },
            Message::text(Role::User, "second"),
            assistant("call_0"),
        ];
        let answered = close_orphaned_calls(&mut history);
        assert_eq!(answered, ["call_0"]);
        assert_eq!(history.len(), 6, "one synthetic result, no duplicates");
        let Some(Content::ToolResult(result)) = history[5].content.first() else {
            panic!("the tail answers the second call");
        };
        assert_eq!(result.id, "call_0");
        assert!(result.is_error);
    }

    #[test]
    fn an_answered_history_is_left_alone() {
        let mut history = vec![
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command": "echo hi"}),
                    provider_data: serde_json::Value::Null,
                })],
            },
            Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::failure(
                    "call-1",
                    "already answered",
                ))],
            },
        ];
        let before = history.clone();
        assert!(close_orphaned_calls(&mut history).is_empty());
        assert_eq!(history.len(), before.len());
    }

    #[test]
    fn only_the_unanswered_calls_are_repaired() {
        let call = |id: &str| {
            Content::ToolCall(ToolCall {
                id: id.into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
                provider_data: serde_json::Value::Null,
            })
        };
        let mut history = vec![
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![call("answered"), call("dangling"), call("duplicated")],
            },
            Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::failure(
                    "answered",
                    "the one that ran",
                ))],
            },
            Message {
                annotations: Default::default(),
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult::failure(
                    "duplicated",
                    "the twin that ran",
                ))],
            },
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![Content::Text("done".into())],
            },
        ];
        let answered = close_orphaned_calls(&mut history);
        assert_eq!(answered, ["dangling"]);
        assert_eq!(history.len(), 5);
    }

    #[test]
    fn a_non_tail_orphan_is_answered_before_later_messages() {
        let call = |id: &str| {
            Content::ToolCall(ToolCall {
                id: id.into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
                provider_data: serde_json::Value::Null,
            })
        };
        let mut history = vec![
            Message::text(Role::User, "run it"),
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![call("dangling")],
            },
            Message::text(Role::User, "later"),
            Message::text(Role::Assistant, "done"),
        ];
        let answered = close_orphaned_calls(&mut history);
        assert_eq!(answered, ["dangling"]);
        assert_eq!(history.len(), 5);
        let Some(Content::ToolResult(result)) = history[2].content.first() else {
            panic!("the result sits directly after its assistant message");
        };
        assert_eq!(result.id, "dangling");
        assert_eq!(
            history[3].role,
            Role::User,
            "later messages keep their order"
        );
        assert_eq!(history[4].role, Role::Assistant);
    }

    #[test]
    fn a_missing_call_keeps_its_place_in_the_result_block() {
        let call = |id: &str| {
            Content::ToolCall(ToolCall {
                id: id.into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo hi"}),
                provider_data: serde_json::Value::Null,
            })
        };
        let result = |id: &str| Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult::failure(id, "ran"))],
        };

        let mut missing_first = vec![
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![call("missing"), call("answered")],
            },
            result("answered"),
            Message::text(Role::Assistant, "done"),
        ];
        assert_eq!(close_orphaned_calls(&mut missing_first), ["missing"]);
        assert_eq!(missing_first.len(), 4);
        let Some(Content::ToolResult(first)) = missing_first[1].content.first() else {
            panic!("the missing call goes before the later result");
        };
        assert_eq!(first.id, "missing");
        let Some(Content::ToolResult(second)) = missing_first[2].content.first() else {
            panic!("the answered call keeps its result");
        };
        assert_eq!(second.id, "answered");

        let mut missing_last = vec![
            Message {
                annotations: Default::default(),
                role: Role::Assistant,
                content: vec![call("answered"), call("missing")],
            },
            result("answered"),
            Message::text(Role::Assistant, "done"),
        ];
        assert_eq!(close_orphaned_calls(&mut missing_last), ["missing"]);
        assert_eq!(missing_last.len(), 4);
        let Some(Content::ToolResult(second)) = missing_last[2].content.first() else {
            panic!("the missing call goes after the earlier result");
        };
        assert_eq!(second.id, "missing");
    }

    #[test]
    fn a_retryable_provider_failure_reads_retryable() {
        assert!(AgentError::Provider(ProviderError::Network("dead link".into())).is_retryable());
        assert!(AgentError::Provider(ProviderError::Timeout).is_retryable());
        assert!(
            AgentError::Provider(ProviderError::Api {
                message: "server error (HTTP 502)".into(),
                status: Some(502),
            })
            .is_retryable()
        );
    }

    #[test]
    fn everything_else_reads_permanent() {
        assert_eq!(AgentError::MissingProvider, AgentError::MissingProvider);
        assert!(!AgentError::MissingProvider.is_retryable());
        assert!(
            !AgentError::Provider(ProviderError::Api {
                message: "rate limit".into(),
                status: Some(429),
            })
            .is_retryable()
        );
        assert!(!AgentError::Commit(CommitError("disk full".into())).is_retryable());
        assert!(!AgentError::MissingProvider.is_retryable());
    }
}

#[cfg(test)]
mod revive_tests {
    use super::*;
    use crate::request::ToolSpec;
    use serde_json::json;

    /// Two tools: `safe_tool` declares Safe, `unsafe_tool` keeps the
    /// conservative default.
    struct Mixed;

    #[async_trait::async_trait]
    impl ToolResolver for Mixed {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![
                ToolSpec::new("safe_tool", "safe to re-run", json!({})),
                ToolSpec::new("unsafe_tool", "must not re-run", json!({})),
            ]
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success(call.id, "re-ran")
        }

        fn replay_safety(&self, name: &str) -> ReplaySafety {
            match name {
                "safe_tool" => ReplaySafety::Safe,
                _ => ReplaySafety::Unsafe,
            }
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            args: json!({}),
            provider_data: serde_json::Value::Null,
        }
    }

    fn session_with_dangling() -> Session {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "go"));
        session.history.push(Message {
            role: Role::Assistant,
            content: vec![
                Content::ToolCall(call("safe_1", "safe_tool")),
                Content::ToolCall(call("unsafe_1", "unsafe_tool")),
            ],
            annotations: Default::default(),
        });
        session
    }

    #[tokio::test]
    async fn revive_reexecutes_safe_calls_and_reports_unsafe_ones() {
        let mut session = session_with_dangling();

        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;

        assert_eq!(session.history.len(), 4);
        let results: Vec<&ToolResult> = session.history[2..]
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                Content::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        // Call order preserved: the safe call's fresh result first.
        assert_eq!(results[0].id, "safe_1");
        assert!(!results[0].is_error);
        assert_eq!(results[0].content, "re-ran");
        assert_eq!(results[1].id, "unsafe_1");
        assert!(results[1].is_error);
        assert!(results[1].content.contains("interrupted"));
        // A revived history is closed: nothing dangles.
        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;
        assert_eq!(session.history.len(), 4);
    }

    #[tokio::test]
    async fn revive_leaves_a_clean_history_alone() {
        let mut session = Session::default();
        session.history.push(Message::text(Role::User, "go"));
        session.history.push(Message::text(Role::Assistant, "done"));

        revive_dangling_calls(&mut session, &Mixed, &ToolContext::default()).await;

        assert_eq!(session.history.len(), 2);
    }
}
