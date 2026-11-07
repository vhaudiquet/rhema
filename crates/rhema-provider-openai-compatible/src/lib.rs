//! `rhema-provider-openai-compatible` — the generic OpenAI-compatible
//! adapter for rhema-core's [`ModelProvider`](rhema_core::ModelProvider)
//! seam.
//!
//! Speaks the OpenAI chat-completions dialect directly: one
//! `POST {base}/chat/completions` with `stream: true` per exchange, the API
//! key in the `Authorization: Bearer` header, the SSE stream decoded into
//! rhema-core [`Event`](rhema_core::Event)s and assembled into a
//! [`ModelOutput`](rhema_core::ModelOutput). `GET {base}/models` lists the
//! server's models as [`ModelCard`](rhema_provider::ModelCard)
//! rows — the shared normalized model type, text chat models only. This is
//! the home for "other
//! base URLs": any server speaking the dialect (OpenAI itself, vLLM, LM
//! Studio, a proxy) is addressed by its full versioned base, e.g.
//! `https://api.openai.com/v1` or a local server's `/v1`. The wire mapping
//! constraints — roles, tool-call fragments, stop reasons, response shapes
//! — are documented in the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_openai_compatible::{DEFAULT_BASE_URL, OpenAiCompatibleProvider};
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider = OpenAiCompatibleProvider::new(std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY is set"));
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("gpt-4o-mini"),
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
//!   [`rhema_core::ModelOutput`] / [`rhema_core::Event`]s, and the models
//!   listing mapping onto `ModelCard`
//! - [`provider`] — the HTTP transport and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]

pub mod provider;
pub mod wire;

pub use provider::{DEEPSEEK_BASE_URL, DEFAULT_BASE_URL, OpenAiCompatibleProvider};
pub use wire::{ChatCompletionChunk, ChatCompletionRequest, ChunkAssembler};
