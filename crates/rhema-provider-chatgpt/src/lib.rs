//! `rhema-provider-chatgpt` is the ChatGPT subscription adapter for
//! rhema-core's [`ModelProvider`](rhema_core::ModelProvider) seam.
//!
//! A ChatGPT plan is not an API key: the account authorizes rhema
//! through OpenAI's OAuth authorization-code flow with PKCE, run in a
//! browser and captured on a loopback callback (see [`login`]). The
//! tokens that flow returns are the credential this provider refreshes
//! and drives the Codex backend. It sends `POST
//! {api}/responses` against `https://chatgpt.com/backend-api/codex`,
//! carrying the bearer access token and the `chatgpt-account-id`
//! header, with the Responses API's SSE events decoded through the
//! shared [`SseDecoder`](rhema_provider::SseDecoder) into rhema-core
//! [`Event`](rhema_core::Event)s. `GET {api}/models` lists the plan's
//! catalog. The wire mapping for input items, reasoning, function
//! calls, usage, and errors is documented in the [`wire`] module.
//!
//! ```
//! use rhema_core::{ChatParams, Event, EventSink, Message, ModelProvider, Role};
//! use rhema_provider_chatgpt::ChatGptProvider;
//!
//! struct Print;
//! impl EventSink for Print {
//!     fn emit(&mut self, event: Event<'_>) { println!("{event:?}"); }
//! }
//!
//! # async fn demo() -> Result<(), rhema_core::ProviderError> {
//! // The login flow's `Completed` string, or a bare access token.
//! let provider = ChatGptProvider::new(std::env::var("CHATGPT_ACCESS_TOKEN").expect("CHATGPT_ACCESS_TOKEN is set"));
//! let request = rhema_core::ModelRequest {
//!     system: Some("be terse".into()),
//!     messages: vec![Message::text(Role::User, "reply with exactly: ok")],
//!     tools: vec![],
//!     params: ChatParams::new("gpt-5.1-codex"),
//! };
//! let mut sink = Print;
//! let output = provider.complete(request, &mut sink).await?;
//! assert_eq!(output.message.role, Role::Assistant);
//! # Ok(())
//! # }
//! ```
//!
//! Layout:
//! - [`credential`]: the JSON credential the login flow returns, its
//!   bare-token fallback, and the JWT claims it reads
//! - [`wire`]: serde wire types plus the pure mapping between
//!   [`rhema_core::ModelRequest`] / Responses SSE events and
//!   [`rhema_core::ModelOutput`] / [`rhema_core::Event`]s, and the
//!   models listing mapping onto [`ModelCard`](rhema_provider::ModelCard)
//! - [`provider`]: the HTTP transport, the access token's refresh and
//!   cache, and the [`ModelProvider`](rhema_core::ModelProvider)
//!   implementation
//! - [`login`]: the browser OAuth flow behind the credential, on the
//!   shared `InteractiveLogin` seam

#![forbid(unsafe_code)]

pub mod credential;
pub mod login;
pub mod provider;
pub mod wire;

pub use credential::ChatGptCredential;
pub use login::{ChatGptLogin, DEFAULT_CLIENT_ID, DEFAULT_ISSUER};
pub use provider::{ChatGptProvider, DEFAULT_API_BASE, MODELS_CLIENT_VERSION};
pub use wire::{ResponsesRequest, StreamAssembler, normalize_models, request_body};
