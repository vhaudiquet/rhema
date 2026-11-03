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
//! public catalog as [`ModelCard`](rhema_provider::ModelCard) rows — the
//! shared normalized model type — with pricing converted from
//! OpenRouter's USD-per-token strings to per-MTok; the text chat models
//! only, since the catalog also aggregates image, audio, and embedding
//! backends. The wire mapping
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
//! let provider = OpenRouterProvider::new(std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY is set"));
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
//! - [`wire`] — OpenRouter's [`Dialect`], the paginated catalog with
//!   per-token pricing, and the credits/auth-key allowance parsing
//! - [`provider`] — the HTTP transport (catalog, allowance) and the
//!   [`ModelProvider`](rhema_core::ModelProvider) implementation,
//!   completing through the shared dialect core

#![forbid(unsafe_code)]

pub mod provider;
pub mod wire;

pub use provider::{APP_REFERER, APP_TITLE, DEFAULT_BASE_URL, OpenRouterProvider};
pub use wire::{OPENROUTER, WireCredits, WireKey, WireModel, WireModelsPage, is_text};
