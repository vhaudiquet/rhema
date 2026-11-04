//! The wire types: JSON-RPC 2.0 envelopes and the MCP subset this
//! client speaks.
//!
//! Response types tolerate unknown fields: servers evolve, and a field
//! this client never asked about must not fail a parse. Request types
//! are exact because they are ours. Content blocks stay flat
//! (`type` discriminator plus the verbatim JSON): the block-type set is
//! server-extensible, and interpretation — text, media, structured —
//! is the caller's job, not the transport's.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol revision this client advertises: a deliberate pin, not
/// "latest". Grown only when a revision's deltas are implemented and
/// reviewed (spec §6).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions a stdio server may answer the handshake with.
pub const SUPPORTED_STDIO: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The revisions a Streamable HTTP server may answer with: one fewer
/// than stdio, because `2024-11-05` delivers responses on a GET SSE
/// stream this client never opens — accepting it there would hang
/// every call (spec §6).
pub const SUPPORTED_HTTP: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// MCP method names, as constants so typos are compile errors.
pub mod method {
    /// The handshake request.
    pub const INITIALIZE: &str = "initialize";
    /// The post-handshake notification (HTTP servers answer it 202).
    pub const INITIALIZED: &str = "notifications/initialized";
    /// The catalog request.
    pub const TOOLS_LIST: &str = "tools/list";
    /// The tool invocation request.
    pub const TOOLS_CALL: &str = "tools/call";
    /// The cancellation notification, sent unconditionally: there is
    /// no capability to negotiate (spec §6).
    pub const CANCELLED: &str = "notifications/cancelled";
}

/// A JSON-RPC request id: a number here, a string on tolerant servers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// The numeric form this client always sends.
    Number(u64),
    /// The string form some servers echo.
    Text(String),
}

/// One tool definition from `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// The tool's name, verbatim.
    pub name: String,
    /// The tool's description, verbatim; feeds `--help`.
    #[serde(default)]
    pub description: Option<String>,
    /// The JSON Schema for the tool's arguments, verbatim.
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// A content block from a `tools/call` result: the `type`
/// discriminator plus the block verbatim, extra fields included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    /// The block type as the server sent it (`text`, `image`,
    /// `audio`, `resource_link`, or whatever the server invented).
    #[serde(rename = "type")]
    pub kind: String,
    /// The whole block, this field included.
    #[serde(flatten)]
    pub raw: serde_json::Map<String, Value>,
}

/// A `tools/call` result. `is_error` is data, not a transport error:
/// the caller decides what a failed tool means.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    /// The content blocks, in server order.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// The server's own failure flag, when it sets one.
    #[serde(default, rename = "isError")]
    pub is_error: Option<bool>,
    /// Structured output, when the server sent it.
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
}

/// A client or server identity as the handshake carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    /// The client's or server's name.
    pub name: String,
    /// The client's or server's version.
    pub version: String,
}

/// The `initialize` request parameters.
#[derive(Debug, Serialize)]
pub struct InitializeParams {
    /// [`PROTOCOL_VERSION`], advertised exactly.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    /// The capabilities object: `{}` — this client negotiates nothing.
    pub capabilities: Value,
    /// Who is calling.
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

/// The `initialize` result.
#[derive(Debug, Deserialize)]
pub struct InitializeResult {
    /// The revision the server picked; checked against the
    /// transport's accept-set.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// The server's capabilities, uninterpreted by this subset.
    #[serde(default)]
    pub capabilities: Value,
    /// Who answered.
    #[serde(default, rename = "serverInfo")]
    pub server_info: Option<Implementation>,
}

/// The `tools/list` request parameters.
#[derive(Debug, Serialize)]
pub struct ToolsListParams {
    /// The continuation cursor, when paginating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// One `tools/list` page.
#[derive(Debug, Deserialize)]
pub struct ToolsListResult {
    /// The page's tools.
    pub tools: Vec<ToolDef>,
    /// The next cursor, absent on the last page.
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// The `tools/call` request parameters.
#[derive(Debug, Serialize)]
pub struct ToolsCallParams {
    /// The tool to invoke.
    pub name: String,
    /// The arguments object the mapper built.
    pub arguments: Value,
}

/// The `notifications/cancelled` parameters.
#[derive(Debug, Serialize, Deserialize)]
pub struct CancelledParams {
    /// The request id being cancelled.
    #[serde(rename = "requestId")]
    pub request_id: RequestId,
}

/// An error carried inside a JSON-RPC response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    /// The JSON-RPC error code.
    pub code: i64,
    /// The server's message.
    pub message: String,
}

/// An outgoing request envelope.
#[derive(Debug, Serialize)]
pub struct RpcRequest<'a> {
    /// The JSON-RPC version, always `"2.0"`.
    jsonrpc: &'static str,
    /// The request id.
    id: RequestId,
    /// The method name.
    method: &'a str,
    /// The parameters, when the method takes any.
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

impl<'a> RpcRequest<'a> {
    /// Builds an envelope with a fresh numeric id.
    pub fn new(id: u64, method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: RequestId::Number(id),
            method,
            params,
        }
    }

    /// The request as one wire line.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("envelope serializes")
    }

    /// The id this envelope carries.
    pub fn id(&self) -> u64 {
        match self.id {
            RequestId::Number(n) => n,
            RequestId::Text(_) => unreachable!("this client always sends numeric ids"),
        }
    }
}

/// An outgoing notification envelope (no id).
#[derive(Debug, Serialize)]
pub struct RpcNotification<'a> {
    /// The JSON-RPC version, always `"2.0"`.
    jsonrpc: &'static str,
    /// The method name.
    method: &'a str,
    /// The parameters, when the method takes any.
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

impl<'a> RpcNotification<'a> {
    /// Builds a notification envelope.
    pub fn new(method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }

    /// The notification as one wire line.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("envelope serializes")
    }
}

/// An incoming message from the server: a response to one of our
/// requests, or a notification. A server-to-client *request* (sampling
/// and friends, out of scope per spec §0) parses as a notification
/// shaped value and is ignored by the routing that finds no pending
/// entry for it.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    /// A response carrying an id.
    Response(Response),
    /// A notification carrying no id.
    Notification(Notification),
}

/// A parsed response envelope: result or error, exactly one.
#[derive(Debug, Deserialize)]
pub struct Response {
    /// The id the server echoed.
    pub id: RequestId,
    /// The success payload, when the call succeeded.
    #[serde(default)]
    pub result: Option<Value>,
    /// The JSON-RPC error, when the call failed.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A parsed notification envelope.
#[derive(Debug, Deserialize)]
pub struct Notification {
    /// The method name.
    pub method: String,
    /// The parameters, when any.
    #[serde(default)]
    pub params: Option<Value>,
}

/// Builds the `initialize` params value for this client.
pub fn initialize_params() -> Value {
    serde_json::to_value(InitializeParams {
        protocol_version: PROTOCOL_VERSION,
        capabilities: Value::Object(serde_json::Map::new()),
        client_info: Implementation {
            name: "rhema".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    })
    .expect("initialize params serialize")
}
