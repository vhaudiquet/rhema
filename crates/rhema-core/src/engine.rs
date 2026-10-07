//! The step engine: [`Agent::step`], one exchange, never a loop.
//!
//! `step()` semantics are exact (SPEC.md §4): emit `StepStart`, assemble the
//! request from the session, stream through the provider, append the
//! assistant message, then — on `ToolUse` — resolve each call in order and
//! append its `Tool`-role result, then emit `StepEnd`. Tool errors never
//! abort; they are in-band `is_error` results. [`run_turn`] is the provided
//! coding-agent driver, not part of the primitive.

use std::collections::HashSet;
use std::sync::Arc;

use crate::event::{Event, EventSink};
use crate::message::{Content, Message, Role};
use crate::output::{ModelOutput, Step, StopReason};
use crate::provider::{ModelProvider, ProviderError};
use crate::request::{ChatParams, ModelRequest};
use crate::session::Session;
use crate::tool::{ToolContext, ToolResolver, ToolResult};

/// Why a step or a turn failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// The agent was built without a [`ModelProvider`].
    #[error("agent built without a model provider")]
    MissingProvider,
    /// The model call failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The turn was still requesting tools after `max_iterations` steps.
    #[error(
        "stopped after {0} steps without finishing — send a follow-up \
             to keep going, or raise the step limit"
    )]
    MaxIterations(u32),
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

/// The step engine: a provider, an optional tool resolver, and the fixed
/// prompt/params it stamps into every request.
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    resolver: Option<Arc<dyn ToolResolver>>,
    system: Option<String>,
    params: ChatParams,
    steps: u32,
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

        let mut events = ForwardingSink {
            inner: sink,
            announced: HashSet::new(),
        };
        events.emit(Event::StepStart { iteration });

        let req = ModelRequest {
            system: self.system.clone(),
            messages: session.history.clone(),
            tools: self
                .resolver
                .as_ref()
                .map(|r| r.specs())
                .unwrap_or_default(),
            params: self.params.clone(),
        };
        let out = self.provider.complete(req, &mut events).await?;

        let call_indices: Vec<usize> = out
            .message
            .content
            .iter()
            .enumerate()
            .filter_map(|(i, c)| matches!(c, Content::ToolCall(_)).then_some(i))
            .collect();
        session.history.push(out.message.clone());

        if out.stop_reason == StopReason::ToolUse {
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
                let result = match &self.resolver {
                    Some(resolver) => resolver.resolve(call.clone(), ctx).await,
                    None => ToolResult::failure(&call.id, "no tool resolver is configured"),
                };
                events.emit(Event::ToolResult {
                    id: &result.id,
                    content: &result.content,
                    is_error: result.is_error,
                    code: result.code,
                });
                session.history.push(Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(result)],
                });
            }
        }

        events.emit(Event::StepEnd {
            usage: out.usage,
            stop_reason: &out.stop_reason,
        });
        Ok(Step { output: out })
    }
}

/// Builder for [`Agent`]; the provider is required, everything else
/// defaults.
#[derive(Default)]
pub struct AgentBuilder {
    provider: Option<Arc<dyn ModelProvider>>,
    resolver: Option<Arc<dyn ToolResolver>>,
    system: Option<String>,
    params: Option<ChatParams>,
}

impl AgentBuilder {
    /// Sets the model backend (required).
    pub fn provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Sets the tool resolver; without one, `specs()` is empty and a `ToolUse`
    /// stop resolves to an in-band error result.
    pub fn resolver(mut self, resolver: Arc<dyn ToolResolver>) -> Self {
        self.resolver = Some(resolver);
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

    /// Builds the agent; fails if no provider was set.
    pub fn build(self) -> Result<Agent, AgentError> {
        Ok(Agent {
            provider: self.provider.ok_or(AgentError::MissingProvider)?,
            resolver: self.resolver,
            system: self.system,
            params: self.params.unwrap_or_default(),
            steps: 0,
        })
    }
}

/// The provided coding-agent driver: push the user input, then loop steps
/// until the model stops requesting tools. Not the primitive — the RSD
/// driver (crate 2) has the same shape with different scaffolding.
pub async fn run_turn(
    agent: &mut Agent,
    session: &mut Session,
    input: Message,
    sink: &mut dyn EventSink,
    ctx: &ToolContext,
    max_iterations: u32,
) -> Result<ModelOutput, AgentError> {
    session.history.push(input);
    for _ in 0..max_iterations {
        let step = agent.step(session, sink, ctx).await?;
        if step.output.stop_reason != StopReason::ToolUse {
            return Ok(step.output);
        }
    }
    Err(AgentError::MaxIterations(max_iterations))
}
