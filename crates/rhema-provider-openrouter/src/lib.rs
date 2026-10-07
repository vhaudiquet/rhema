//! `rhema-provider-openrouter` — the OpenRouter adapter for rhema-core's
//! [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! OpenRouter (openrouter.ai) aggregates hosted models behind an
//! OpenAI-compatible chat-completions API. Speaks it directly: one
//! `POST {base}/chat/completions` with `stream: true` per exchange, the API
//! key in the `Authorization: Bearer` header (plus the recommended
//! `HTTP-Referer`/`X-Title` attribution headers), the SSE stream decoded
//! into rhema-core [`Event`](rhema_core::Event)s and assembled into a
//! [`ModelOutput`](rhema_core::ModelOutput). `GET {base}/models` lists the
//! public catalog as [`GeminiModel`](rhema_provider_google::GeminiModel)
//! rows — the shared normalized model type — with pricing converted from
//! OpenRouter's USD-per-token strings to per-MTok. The wire mapping
//! constraints, including OpenRouter's deviations from the OpenAI dialect,
//! are documented in the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_openrouter::{DEFAULT_BASE_URL, OpenRouterProvider};
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider = OpenRouterProvider::new(std::env::var("OPENROUTER_API_KEY").unwrap());
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("openai/gpt-4o-mini"),
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
//!   catalog mapping onto `GeminiModel`
//! - [`stream`] — the SSE byte-stream decoder
//! - [`provider`] — the HTTP transport and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod provider;
pub mod stream;
pub mod wire;

pub use provider::{APP_REFERER, APP_TITLE, DEFAULT_BASE_URL, OpenRouterProvider};
pub use stream::SseDecoder;
pub use wire::{ChatCompletionChunk, ChatCompletionRequest, ChunkAssembler};
