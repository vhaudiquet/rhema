//! rhema-core — a small, embeddable, generic step engine for model exchanges.
//!
//! The primitive is `Agent::step` (coming in the engine module): ONE
//! exchange with a model — build the request from the caller-owned session,
//! stream through a model provider, append the assistant message, resolve
//! tool calls, append the results. The engine never loops, never persists;
//! those belong to drivers (SPEC.md §0, §5).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod message;
pub mod request;
pub mod tool;

pub use message::{Content, Message, Role};
pub use request::{ChatParams, ModelRequest, ToolSpec};
pub use tool::{ToolCall, ToolContext, ToolResolver, ToolResult};
