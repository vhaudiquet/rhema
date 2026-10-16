//! `rhema-provider-llama-cpp` — the llama.cpp `llama-server` adapter for
//! rhema-core's [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! Speaks the server's OpenAI-compatible chat-completions API directly: one
//! `POST {base}/v1/chat/completions` with `stream: true` per exchange, an
//! optional API key in the `Authorization: Bearer` header, the SSE stream
//! decoded into rhema-core [`Event`](rhema_core::Event)s and assembled into
//! a [`ModelOutput`](rhema_core::ModelOutput). The wire mapping constraints
//! — roles, tool-call fragments, stop reasons, response shapes — are
//! documented in the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_llama_cpp::{DEFAULT_BASE_URL, LlamaCppProvider};
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider = LlamaCppProvider::new(DEFAULT_BASE_URL);
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("qwen2.5-coder-7b"),
//! };
//! let mut sink = Print;
//! let output = provider.complete(request, &mut sink).await?;
//! assert_eq!(output.message.role, Role::Assistant);
//! # Ok(())
//! # }
//! ```
//!
//! Layout:
//! - [`wire`] — serde wire types plus the pure mapping between
//!   [`rhema_core::ModelRequest`] / SSE chunks and
//!   [`rhema_core::ModelOutput`] / [`rhema_core::Event`]s
//! - [`provider`] — the HTTP transport and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod provider;
pub mod wire;

pub use provider::{DEFAULT_BASE_URL, LlamaCppProvider};
pub use wire::{
    ChatCompletionChunk, ChatCompletionRequest, ChunkAssembler, LlamaModel, context_from_props,
    normalize_models,
};
