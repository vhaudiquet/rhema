//! `rhema-provider-anthropic` — the Anthropic Messages API adapter for
//! rhema-core's [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! Speaks the Messages REST API directly: one `POST {base}/v1/messages`
//! with `stream: true` per exchange, the API key in the `x-api-key` header
//! (or `Authorization: Bearer` via `with_bearer_auth`, for the
//! Anthropic-compatible endpoints such as Z.ai's GLM Coding Plan route,
//! [`ZAI_BASE_URL`](provider::ZAI_BASE_URL)) and the pinned
//! `anthropic-version` header, the SSE event stream decoded
//! into rhema-core [`Event`](rhema_core::Event)s and assembled into a
//! [`ModelOutput`](rhema_core::ModelOutput). `GET {base}/v1/models` lists
//! the account's models as
//! [`ModelCard`](rhema_provider::ModelCard) rows — the shared
//! normalized model type; Anthropic's listing is text chat models only.
//! The wire mapping constraints — user-turn tool
//! results, the split usage accounting, stop reasons — are documented in
//! the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_anthropic::AnthropicProvider;
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider = AnthropicProvider::new(std::env::var("ANTHROPIC_API_KEY").unwrap());
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("claude-sonnet-4-5"),
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
//!   [`rhema_core::ModelRequest`] / SSE events and
//!   [`rhema_core::ModelOutput`] / [`rhema_core::Event`]s, and the models
//!   mapping onto `ModelCard`
//! - [`provider`] — the HTTP transport and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod provider;
pub mod wire;

pub use provider::{ANTHROPIC_VERSION, AnthropicProvider, DEFAULT_BASE_URL, ZAI_BASE_URL};
pub use wire::{ChunkAssembler, DEFAULT_MAX_TOKENS, MessagesRequest, StreamEvent};
