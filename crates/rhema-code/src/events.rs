//! The protocol between the turn task and the UI thread.
//!
//! The turn task never touches the terminal: everything it wants the user
//! to see travels as a [`UiEvent`] over an unbounded tokio mpsc channel.
//! Unbounded because [`rhema_core::EventSink::emit`] is synchronous and
//! cannot await — a bounded, back-pressuring channel would deadlock the
//! stream mid-turn.

use rhema_coding_agent::CodingAgent;
use rhema_core::{Event, ModelOutput, ToolCall, Usage};
use rhema_provider::ModelCard;

/// Why the runner fetched the model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchWhy {
    /// The login screen validated a new instance's input; the fetch is
    /// the validation (`Login` carries the backend the screen assembled
    /// — key or server URL — and the instance name it will persist the
    /// credential under; nothing else stores them mid-flight).
    Login {
        /// The backend being validated.
        backend: crate::app::Backend,
        /// The name the credential lands under in the profile's
        /// instances.
        instance: Option<String>,
    },
    /// Startup with a resolvable key; populates the picker (and the Ctrl-P
    /// cache) without blocking the first paint.
    Bootstrap,
    /// The model picker reopened mid-session and its cache is stale.
    Refresh,
}

/// One fetched catalog slice: which backend answered, and with what.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogFetch {
    /// The backend's provider config name.
    pub provider: &'static str,
    /// The profile instance the backend was built from, when it has a
    /// name — every stored instance does; only an environment-only
    /// fetch is unnamed.
    pub instance: Option<String>,
    /// The models, or the provider's error text.
    pub result: Result<Vec<ModelCard>, String>,
}

/// An owned copy of a [`rhema_core::Event`]. [`Event`] borrows from the
/// stream; the channel needs `'static`.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedEvent {
    /// See [`Event::StepStart`].
    StepStart {
        /// Zero-based step index within the agent.
        iteration: u32,
    },
    /// See [`Event::ThinkingDelta`].
    ThinkingDelta(
        /// The streamed reasoning piece.
        String,
    ),
    /// See [`Event::TextDelta`].
    TextDelta(
        /// The streamed text piece.
        String,
    ),
    /// See [`Event::ToolCallStart`].
    ToolCallStart {
        /// The call's correlation id.
        id: String,
        /// The tool's name.
        name: String,
    },
    /// See [`Event::ToolCallArgsDelta`].
    ToolCallArgsDelta {
        /// The streaming call's correlation id.
        id: String,
        /// A piece of the call's JSON arguments.
        delta: String,
    },
    /// See [`Event::ToolOutputDelta`].
    ToolOutputDelta {
        /// The running call's correlation id.
        id: String,
        /// A chunk of the tool's output; complete lines, newline
        /// included.
        chunk: String,
    },
    /// See [`Event::ToolStatus`].
    ToolStatus {
        /// The running call's correlation id.
        id: String,
        /// The tool's status, replacing any earlier one for the call.
        status: rhema_core::ToolStatus,
    },
    /// See [`Event::ToolResult`].
    ToolResult {
        /// The answered call's id.
        id: String,
        /// Result payload.
        content: String,
        /// Whether the call failed.
        is_error: bool,
        /// The process exit code of command tools; `None` otherwise.
        code: Option<i32>,
    },
    /// See [`Event::Usage`].
    Usage(
        /// The running cumulative tally for the in-flight call.
        Usage,
    ),
    /// See [`Event::RetryWait`].
    RetryWait {
        /// The failed attempt's error text.
        error: String,
        /// How long the ladder waits before the next attempt.
        delay: std::time::Duration,
        /// One-based number of the attempt that just failed.
        attempt: u32,
        /// Total attempts the policy will spend.
        total: u32,
    },
    /// See [`Event::StepEnd`].
    StepEnd {
        /// Token accounting for the model call.
        usage: Usage,
        /// Why the model stopped.
        stop_reason: rhema_core::StopReason,
    },
}

impl From<&Event<'_>> for OwnedEvent {
    fn from(event: &Event<'_>) -> Self {
        match event {
            Event::StepStart { iteration } => Self::StepStart {
                iteration: *iteration,
            },
            Event::ThinkingDelta(delta) => Self::ThinkingDelta((*delta).to_owned()),
            Event::TextDelta(delta) => Self::TextDelta((*delta).to_owned()),
            Event::ToolCallStart { id, name } => Self::ToolCallStart {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
            },
            Event::ToolCallArgsDelta { id, delta } => Self::ToolCallArgsDelta {
                id: (*id).to_owned(),
                delta: (*delta).to_owned(),
            },
            Event::ToolOutputDelta { id, chunk } => Self::ToolOutputDelta {
                id: (*id).to_owned(),
                chunk: (*chunk).to_owned(),
            },
            Event::ToolStatus { id, status } => Self::ToolStatus {
                id: (*id).to_owned(),
                status: (*status).clone(),
            },
            Event::ToolResult {
                id,
                content,
                is_error,
                code,
            } => Self::ToolResult {
                id: (*id).to_owned(),
                content: (*content).to_owned(),
                is_error: *is_error,
                code: *code,
            },
            Event::Usage(usage) => Self::Usage(*usage),
            Event::RetryWait {
                error,
                delay,
                attempt,
                total,
            } => Self::RetryWait {
                error: (*error).to_owned(),
                delay: *delay,
                attempt: *attempt,
                total: *total,
            },
            Event::StepEnd { usage, stop_reason } => Self::StepEnd {
                usage: *usage,
                stop_reason: (*stop_reason).clone(),
            },
        }
    }
}

/// One message from the turn/fetch/login tasks to the UI loop.
pub enum UiEvent {
    /// A streaming or lifecycle event of the running turn.
    Engine(OwnedEvent),
    /// A model-list fetch completed.
    ModelsLoaded {
        /// What the fetch was for; the app routes the result accordingly.
        why: FetchWhy,
        /// Per backend queried: provider, instance, and the fetched
        /// models or the provider's error text.
        result: Vec<CatalogFetch>,
    },
    /// A subscription-allowance fetch completed.
    UsageLoaded {
        /// The backend's config name the fetch ran for: a report of a
        /// backend the session has since left never shows.
        provider: &'static str,
        /// The allowance, `None` when the backend reports no such
        /// thing; `Err` when the fetch failed.
        result: Result<Option<rhema_provider::Allowance>, String>,
    },
    /// A step of the running login flow: an instruction to render, a
    /// progress note, the credential to persist, or the failure. The
    /// protocol specifics stay in the flow; the app only routes these.
    LoginUpdate(rhema_provider::LoginUpdate),
    /// A gated tool call is waiting for the user's answer. The turn task
    /// blocks until the UI answers (see `crate::approval`).
    ApprovalRequested {
        /// The call seeking approval.
        call: ToolCall,
    },
    /// The turn task finished and hands the agent back to the UI thread.
    TurnFinished {
        /// The agent, with the turn's messages already appended.
        agent: Box<CodingAgent>,
        /// The final output, or the error text of the failed turn.
        result: Result<ModelOutput, String>,
        /// The steering channel's receiver, returned so a message sent in
        /// the finishing race — after the task's last poll, before the UI
        /// saw this event — sits buffered here instead of vanishing. The
        /// app drains it and runs what it holds as the next turn.
        steer: tokio::sync::mpsc::Receiver<String>,
    },
    /// The turn was cancelled from the keyboard and hands the agent back.
    /// Whatever committed before the drop stays in the history; dangling
    /// tool calls are already repaired with failure results.
    TurnCancelled {
        /// The agent, with the partial turn's committed messages.
        agent: Box<CodingAgent>,
        /// The steering channel's receiver, as in
        /// [`UiEvent::TurnFinished::steer`]; the app puts what it holds
        /// back in the input box, since the user stopped the turn.
        steer: tokio::sync::mpsc::Receiver<String>,
    },
    /// The sandbox assembly the picker's choice spawned finished: the
    /// assembled tool set, or the failure text. The session's current
    /// tools keep running either way until the app swaps them.
    SandboxAssembled {
        /// The choice the assembly stood up.
        choice: crate::args::SandboxChoice,
        /// The assembled tool set and its sandbox-side workspace, or
        /// why the choice could not be stood up.
        result: Result<crate::sandbox::Sandboxed, String>,
    },
}
