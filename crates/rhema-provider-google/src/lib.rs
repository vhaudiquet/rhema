//! `rhema-provider-google` — the Google Gemini adapter for rhema-core's
//! [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! Speaks the Generative Language REST API directly: one
//! `POST {base}/models/{model}:streamGenerateContent?alt=sse` per exchange,
//! the API key in the `x-goog-api-key` header, the SSE stream decoded into
//! rhema-core [`Event`](rhema_core::Event)s and assembled into a
//! [`ModelOutput`](rhema_core::ModelOutput). The wire mapping constraints —
//! roles, tool-call ids, stop reasons, response shapes — are documented in
//! the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_google::GoogleProvider;
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider = GoogleProvider::new(std::env::var("GEMINI_API_KEY").unwrap());
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("gemini-3.8-flash"),
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
//! - [`stream`] — the SSE byte-stream decoder
//! - [`provider`] — the HTTP transport and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod provider;
pub mod stream;
pub mod wire;

pub use provider::{DEFAULT_BASE_URL, GoogleProvider};
pub use stream::SseDecoder;
pub use wire::{
    ChunkAssembler, GeminiModel, GenerateContentRequest, GenerateContentResponse, NormalizedPart,
};
