//! rhema-core — a small, embeddable, generic step engine for model exchanges.
//!
//! The primitive is [`Agent::step`]: ONE exchange with a model. Build the
//! request from the caller-owned [`Session`], stream it through a
//! [`ModelProvider`], append the assistant message to the session, resolve
//! any tool calls through a [`ToolResolver`], and append the results. The
//! engine never loops, never dispatches, never persists — those belong to
//! drivers (SPEC.md §0, §5).
//!
//! Layout:
//! - [`message`] — `Role`, `Content`, `Message`
//! - [`session`] — the caller-owned, serializable `Session`
//! - [`request`] — `ModelRequest`, `ChatParams`, `ToolSpec`
//! - [`output`] — `ModelOutput`, `StopReason`, `Usage`, `Step`
//! - [`provider`] — the `ModelProvider` seam
//! - [`tool`] — the `ToolResolver` seam
//! - [`event`] — `Event`, `EventSink`
//! - [`engine`] — the `Agent` step engine, `AgentBuilder`, `run_turn`
//! - [`mock`] — scripted `MockProvider` (behind the `mock` feature)

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod engine;
pub mod event;
pub mod message;
#[cfg(feature = "mock")]
pub mod mock;
pub mod output;
pub mod provider;
pub mod request;
pub mod session;
pub mod tool;

// Re-exported so implementors of the async seams can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use engine::{Agent, AgentBuilder, AgentError, run_turn};
pub use event::{Event, EventSink};
pub use message::{Content, Message, Role};
#[cfg(feature = "mock")]
pub use mock::MockProvider;
pub use output::{ModelOutput, Step, StopReason, Usage};
pub use provider::{ModelProvider, ProviderError};
pub use request::{ChatParams, ModelRequest, ToolSpec};
pub use session::Session;
pub use tool::{ToolCall, ToolContext, ToolResolver, ToolResult};
