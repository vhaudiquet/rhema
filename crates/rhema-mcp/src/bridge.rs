//! The bridge: the manager's HTTP face over a Unix socket.
//!
//! The generated commands live in throwaway bash processes; the
//! manager and its live server connections live in the agent process;
//! this bridge is the wire between them. axum over
//! [`tokio::net::UnixListener`], bound `0600` after bind (kernel
//! connect permissions do the real work; the bearer token is cheap
//! insurance beside it, the same pair the claw's resident server
//! uses).
//!
//! Routes follow spec §5's surface: `/servers` (no spawns), `/tools`
//! (names and descriptions only — the disclosure boundary: schemas
//! never pass this route), `/catalog` (full schemas, the CLI's
//! clap-building input), `/schema`, and `/call`. Bodies cap at 1 MiB.
//!
//! Cancellation is the interesting one: when a bridge client
//! disconnects mid-call, axum drops the handler future — a drop guard
//! fires the request's [`Cancel`], the client drops its pending entry
//! and sends `notifications/cancelled` unconditionally, and a late
//! result is discarded by construction.

use std::convert::Infallible;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::net::UnixListener;

use crate::client::Cancel;
use crate::manager::McpManager;

/// One request body's ceiling: the spec's 1 MiB, aligned with the
/// bridge cap the generated commands are documented against.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The bridge's state: the manager it fronts and the token it checks.
#[derive(Clone)]
struct BridgeState {
    manager: Arc<McpManager>,
    token: String,
}

/// A live bridge. Dropping it removes the socket file and stops
/// serving; keep it for the session's lifetime.
pub struct BridgeHandle {
    socket_path: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl BridgeHandle {
    /// The socket path this bridge serves on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stops serving and removes the socket file.
    pub fn shutdown(self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Binds the bridge on `socket_path` and serves until the handle
/// drops. The socket's parent directory must exist; the file is
/// removed on bind-or-fail of a stale path (a previous session's
/// leftover blocks the bind).
///
/// # Errors
///
/// Socket bind failures (stale path that cannot be removed,
/// unwritable directory).
pub async fn serve(
    manager: Arc<McpManager>,
    socket_path: PathBuf,
    token: String,
) -> std::io::Result<BridgeHandle> {
    // A leftover socket from a crashed session blocks the bind;
    // remove it and let the bind fail on anything that is not ours.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let state = BridgeState { manager, token };
    let app = Router::new()
        .route("/servers", get(servers))
        .route("/tools/{server}", get(tools))
        .route("/catalog/{server}", get(catalog))
        .route("/schema/{server}/{tool}", get(schema))
        .route(
            "/call",
            post(call).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state);

    let path_for_task = socket_path.clone();
    let server = tokio::spawn(async move {
        let listener_for_task = listener;
        if let Err(err) = axum::serve(listener_for_task, app).await {
            eprintln!("[mcp] bridge stopped: {err}");
        }
        let _ = std::fs::remove_file(&path_for_task);
    });

    Ok(BridgeHandle {
        socket_path,
        server,
    })
}

/// The bearer check, constant-time on mismatch — the token is cheap
/// insurance, but it need not be a timing oracle either.
async fn require_auth(
    State(state): State<BridgeState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(presented) if tokens_match(presented, &state.token) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or wrong bearer token"})),
        )
            .into_response(),
    }
}

fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    let mut diff = presented.len() ^ expected.len();
    for (a, b) in presented.iter().zip(expected) {
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

async fn servers(State(state): State<BridgeState>) -> Response {
    Json(state.manager.statuses().await).into_response()
}

/// Names and descriptions only: the disclosure boundary. Schemas ride
/// `/catalog` into the CLI process, never this route into the prompt.
async fn tools(
    State(state): State<BridgeState>,
    axum::extract::Path(server): axum::extract::Path<String>,
) -> Response {
    match state.manager.catalog(&server).await {
        Ok(catalog) => Json(
            catalog
                .iter()
                .map(|tool| json!({"name": tool.name, "description": tool.description}))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => error_response(&server, err),
    }
}

async fn catalog(
    State(state): State<BridgeState>,
    axum::extract::Path(server): axum::extract::Path<String>,
) -> Response {
    match state.manager.catalog(&server).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(err) => error_response(&server, err),
    }
}

async fn schema(
    State(state): State<BridgeState>,
    axum::extract::Path((server, tool)): axum::extract::Path<(String, String)>,
) -> Response {
    match state.manager.catalog(&server).await {
        Ok(catalog) => match catalog.iter().find(|tool_def| tool_def.name == tool) {
            Some(tool_def) => Json(tool_def.input_schema.clone()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("unknown tool `{tool}` on `{server}`"), "kind": "unknown_tool"})),
            )
                .into_response(),
        },
        Err(err) => error_response(&server, err),
    }
}

/// One tool call, answered as an SSE stream: `result` carries the
/// envelope, `error` carries a manager failure, and a keepalive
/// comment rides between.
///
/// The stream, not the handler, is the cancellation point. hyper's
/// HTTP/1 dispatch does not abort an in-flight handler when the
/// client hangs up (the request ran to completion the first time this
/// was built, and the caller's kill left the server busy) — but it
/// DOES drop a response body's future on the first failed write. So
/// the keepalive exists to be written: the caller's death surfaces as
/// a write error within one tick, the stream future (which owns the
/// [`CancelOnDrop`]) is dropped, and the cancellation reaches the
/// server exactly as the spec requires. A CLI killed by the shell's
/// 120s timeout therefore does not leave a playwright browser busy
/// for another two minutes.
async fn call(
    State(state): State<BridgeState>,
    Json(body): Json<CallBody>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let server_for_errors = body.server.clone();
    let cancel = Cancel::new();
    let call_cancel = cancel.clone();
    let manager = state.manager.clone();
    let task = tokio::spawn(async move {
        manager
            .call(
                &body.server,
                &body.tool,
                body.arguments.unwrap_or_default(),
                &call_cancel,
            )
            .await
    });
    let stream = futures_stream(stream_state(cancel, task, server_for_errors));
    Sse::new(stream)
}

/// The `/call` event stream: keepalives until the call task answers,
/// then one terminal event. The guard lives here — dropped the moment
/// hyper drops this future.
async fn stream_state(
    cancel: Cancel,
    mut task: tokio::task::JoinHandle<
        Result<crate::protocol::CallToolResult, crate::manager::ManagerError>,
    >,
    server: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let _guard = CancelOnDrop(cancel);
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep(KEEPALIVE_TICK) => {
                    yield Ok(Event::default().comment("keepalive"));
                }
                answer = &mut task => {
                    match answer {
                        Ok(Ok(result)) => {
                            let body = serde_json::to_string(&result)
                                .unwrap_or_else(|_| "{}".to_owned());
                            // Three event names, three meanings: a
                            // tool-reported failure keeps its
                            // envelope (`tool_error`, the CLI prints
                            // it and exits 5); manager errors carry
                            // a kind string instead (`error`).
                            let event = if result.is_error == Some(true) {
                                "tool_error"
                            } else {
                                "result"
                            };
                            yield Ok(Event::default().event(event).data(body));
                        }
                        Ok(Err(err)) => {
                            let kind = error_kind(&err);
                            yield Ok(Event::default().event("error").data(
                                serde_json::json!({
                                    "error": err.to_string(),
                                    "kind": kind,
                                    "server": server,
                                })
                                .to_string(),
                            ));
                        }
                        Err(join_err) => {
                            yield Ok(Event::default().event("error").data(
                                serde_json::json!({
                                    "error": format!("call task failed: {join_err}"),
                                    "kind": "internal",
                                    "server": server,
                                })
                                .to_string(),
                            ));
                        }
                    }
                    break;
                }
            }
        }
    }
}

/// Wraps the state future as a stream; a naming seam so the handler's
/// return type stays legible.
fn futures_stream(
    state: impl Future<Output = impl Stream<Item = Result<Event, Infallible>>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let inner = state.await;
        for await item in inner {
            yield item;
        }
    }
}

/// The keepalive cadence: fast enough that a dead caller is noticed
/// within a second, slow enough to be noise over a call's lifetime.
const KEEPALIVE_TICK: Duration = Duration::from_millis(500);

/// The `/call` request body: server, tool, and optional argument
/// object.
#[derive(Debug, Deserialize)]
struct CallBody {
    /// The server's config name.
    server: String,
    /// The tool's MCP name.
    tool: String,
    /// The tool's arguments; absent reads as `{}`.
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

/// Fires the token when the handler future dies — return, panic, or
/// axum dropping it on client disconnect.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Manager errors to HTTP: the status IS the CLI's exit-code mapping
/// (404 → 3 unknown server, 422 → 5 server-reported failure, 504 → 6
/// timeout, 502 → 4 transport), the body carries the message.
fn error_response(server: &str, err: crate::manager::ManagerError) -> Response {
    let (status, kind) = error_status_kind(&err);
    (
        status,
        Json(json!({
            "error": err.to_string(),
            "kind": kind,
            "server": server,
        })),
    )
        .into_response()
}

/// Manager errors to (HTTP status, kind) — the status IS the CLI's
/// exit-code mapping (404 → 3 unknown server, 502 → 4 transport, 504
/// → 6 timeout).
fn error_status_kind(err: &crate::manager::ManagerError) -> (StatusCode, &'static str) {
    match err {
        crate::manager::ManagerError::UnknownServer(_) => (StatusCode::NOT_FOUND, "unknown_server"),
        crate::manager::ManagerError::Cooling { .. } => (StatusCode::BAD_GATEWAY, "cooling"),
        crate::manager::ManagerError::Client(client_err) => match client_err {
            crate::ClientError::Rpc { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "rpc"),
            crate::ClientError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
            crate::ClientError::Cancelled => (StatusCode::BAD_REQUEST, "cancelled"),
            _ => (StatusCode::BAD_GATEWAY, "transport"),
        },
    }
}

/// The short kind string an SSE error event carries.
fn error_kind(err: &crate::manager::ManagerError) -> &'static str {
    error_status_kind(err).1
}
