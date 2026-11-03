//! rhema-acp — the Agent Client Protocol adapter: rhema's coding agent
//! as an ACP agent subprocess.
//!
//! ACP (agentclientprotocol.com) is to coding agents what LSP is to
//! language servers: one JSON-RPC protocol, spoken over stdio, between
//! an editor (the *client*) and an agent subprocess. Serving it makes
//! rhema a first-class agent in every ACP client — Zed, JetBrains,
//! Neovim's CodeCompanion, Emacs agent-shell, the VS Code extensions —
//! without a plugin per editor.
//!
//! The adapter is a *frontend*, the third consumer of
//! [`CodingAgent`](rhema_coding_agent::CodingAgent) after the TUI and
//! the headless one-shot: sessions carry the model choice and workspace
//! (`session/new`'s `cwd`), a prompt is one [`turn`](rhema_coding_agent::CodingAgent::turn),
//! engine events stream to the client as `session/update` notifications,
//! the approval seam is answered by `session/request_permission`, and a
//! turn drop repairs dangling tool calls — every seam the TUI already
//! exercises, none of them new. The session's spending is reported
//! too: context window, cost, and per-turn token totals.
//!
//! Layout:
//! - `serve` — the agent surface: handlers, session slots, `serve()`
//! - [`backend`] — provider/model resolution from stored config + flags
//! - [`store`] — session persistence keyed by the ACP session id
//! - `sink` — engine events to `session/update`, and the session's
//!   usage meter (crate-private)
//! - `approval` — `Approval` to `session/request_permission`
//!   (crate-private)
//!
//! Wire discipline: stdin/stdout carry only ACP frames (the SDK owns
//! them); everything rhema has to say goes to stderr. The SDK speaks
//! protocol v1, the stable baseline every current client implements.

#![forbid(unsafe_code)]

pub mod backend;
pub mod serve;
pub mod store;

mod approval;
mod sink;

pub use serve::{AcpOptions, serve, serve_over};
