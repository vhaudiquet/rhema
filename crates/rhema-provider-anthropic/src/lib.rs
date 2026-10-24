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
//! let provider = AnthropicProvider::new(std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY is set"));
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
//! - [`transport`] — the native reqwest transport (feature `native`);
//!   wasm builds turn the feature off and hand the provider their own
//!   [`Transport`](rhema_provider::Transport) over host fetch
//! - [`provider`] — the dialect over the transport seam and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation

#![forbid(unsafe_code)]

pub mod provider;
#[cfg(feature = "native")]
pub mod transport;
pub mod wire;

pub use provider::{
    ANTHROPIC_VERSION, AnthropicProvider, DEFAULT_BASE_URL, USER_AGENT, ZAI_BASE_URL,
};
#[cfg(feature = "native")]
pub use transport::ReqwestTransport;
pub use wire::{ChunkAssembler, DEFAULT_MAX_TOKENS, MessagesRequest, StreamEvent};
