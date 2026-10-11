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
    /// name — every stored instance does; only a legacy flat login
    /// fetches unnamed.
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
    ToolCallArgsDelta(
        /// A piece of the call's JSON arguments.
        String,
    ),
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
            Event::ToolCallArgsDelta(delta) => Self::ToolCallArgsDelta((*delta).to_owned()),
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
    },
    /// The turn was cancelled from the keyboard and hands the agent back.
    /// Whatever committed before the drop stays in the history; dangling
    /// tool calls are already repaired with failure results.
    TurnCancelled {
        /// The agent, with the partial turn's committed messages.
        agent: Box<CodingAgent>,
    },
}
