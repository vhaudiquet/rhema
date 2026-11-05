//! `rhema-provider-copilot` — the GitHub Copilot adapter for
//! rhema-core's [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! Copilot's chat models sit behind an OpenAI-shaped proxy that takes
//! no personal token: a long-lived GitHub OAuth token — produced by the
//! [`login`] module's device flow — is exchanged for a short-lived
//! Copilot token the provider caches and refreshes ahead of expiry, and
//! the proxy's `endpoints.api` base serves the chat-completions calls
//! (`POST {api}/chat/completions`, `GET {api}/models`, decoded through
//! the shared [`SseDecoder`](rhema_provider::SseDecoder) into
//! rhema-core [`Event`](rhema_core::Event)s). The wire mapping
//! constraints — roles, tool-call fragments, stop reasons, the
//! capabilities-based models filter — are documented in the [`wire`]
//! module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_copilot::CopilotProvider;
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! let provider =
//!     CopilotProvider::new(std::env::var("GITHUB_COPILOT_TOKEN").expect("GITHUB_COPILOT_TOKEN is set"));
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("gpt-4.1"),
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
//!   [`rhema_core::ModelOutput`] / [`rhema_core::Event`]s, and the
//!   models listing mapping onto `ModelCard`
//! - [`provider`] — the HTTP and SSE transport, the short-lived token's
//!   exchange and cache, and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation
//! - [`login`] — the GitHub device flow behind the OAuth token, on the
//!   shared `InteractiveLogin` seam

#![forbid(unsafe_code)]

pub mod login;
pub mod provider;
pub mod wire;

pub use login::{CopilotLogin, DEFAULT_CLIENT_ID};
pub use provider::{CopilotProvider, DEFAULT_API_BASE, DEFAULT_GITHUB_API_URL, DEFAULT_GITHUB_URL};
pub use wire::{WireCopilotUser, allowance as wire_allowance};
