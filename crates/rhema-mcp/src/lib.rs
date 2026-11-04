//! rhema-mcp — the MCP client extension: configured MCP servers become
//! bash commands, not advertised tools.
//!
//! The protocol subset (`initialize`, `tools/list`, `tools/call`)
//! rides two transports: a spawned stdio child and Streamable HTTP
//! POSTs. One [`client::Client`] is one live connection; the
//! [`manager`] owns one per configured server, and the [`bridge`]
//! serves the `rhema mcp` CLI processes that turn tool catalogs into
//! clap commands via [`mcp-to-cli`](https://docs.rs/mcp-to-cli).
//!
//! Design contract: specs/rhema-mcp.md. The revision pin is client
//! policy (`2025-06-18`, transport-dependent accept-set), framing is
//! capped (a hostile server can never turn this process into an
//! unbounded reader), and secrets never appear in error strings.
//!
//! Layout:
//! - [`protocol`] — the wire types: JSON-RPC envelopes and the MCP
//!   subset, tolerant of unknown fields
//! - [`client`] — one connection over one transport: handshake,
//!   pending map, per-request timeouts, cancellation, the HTTP
//!   session dance
//!
//! Later commits add `wire.rs` per the spec's commit sequence.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod cli;
pub mod client;
pub mod manager;
pub mod protocol;
pub mod provision;

pub use client::{Cancel, Client, ClientError, HttpParams, StdioParams, Transport};
pub use manager::{ManagerError, McpManager, ServerStatus, SlotSummary};
