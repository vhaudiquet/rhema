//! The wire contract between the rhema claw daemon and its thin client
//! tools — the entire knowledge the tools project may have of the claw.
//!
//! The claw's model-facing data tools are executables the agent runs in
//! its shell: parse arguments, make one HTTP call, print JSON, exit.
//! The daemon keeps the state; the clients hold nothing. What binds the
//! two sides without binding either to the other's crate is this
//! contract: the routes, the JSON shapes, the agent token's allowlist,
//! and the conformance suite both sides answer to. The dependency
//! arrows run claw → contract ← tools and never cross.
//!
//! One decision shapes everything else: the surface reuses the claw's
//! existing `/v1` HTTP API — the same routes the web UI and the phone
//! client speak — so promoting a tool to a route serves the other
//! clients too. The agent token (a second, scoped bearer) is what makes
//! handing the model a credential safe: it answers only the data routes
//! in [`agent_allowed`], never consents, sessions, or ingest, so a tool
//! cannot answer its own `ask_secret`.

use serde::{Deserialize, Serialize};

/// The request header carrying the contract version a client speaks.
pub const CONTRACT_HEADER: &str = "x-rhema-contract";

/// The contract version this crate speaks. A daemon answering at a
/// different version rejects the request with `409` and the standard
/// error shape, so a stale installed binary fails loudly on first use.
pub const CONTRACT_VERSION: u32 = 1;

/// The notifications collection: `GET` lists the queue, `POST` pushes
/// one row a device delivers at its next poll-and-ack.
pub const NOTIFICATIONS: &str = "/v1/notifications";

/// The scheduled tasks: `GET` lists, `POST` schedules, and
/// `DELETE /v1/tasks/{id}` cancels.
pub const TASKS: &str = "/v1/tasks";

/// The forge watch table: `GET` lists, `POST` adds, and
/// `DELETE /v1/watches/{forge}/{repo}` removes.
pub const WATCHES: &str = "/v1/watches";

/// The route allowlist the agent token answers for. The claw's auth
/// middleware and the mock router both run this — one list, so the
/// daemon's enforcement and the contract's conformance suite cannot
/// drift. `path` is the daemon-side path under which the routes nest.
///
/// Reads and writes over the data domains the clients serve: watches,
/// notifications, tasks. Everything else — consent answers, session
/// creation, ingest, phone or account removal — is human-token
/// territory: an agent that could answer consents could answer its own
/// `ask_secret`.
pub fn agent_allowed(method: &str, path: &str) -> bool {
    /// One allowlist row: the method and the path segments, `*` matching
    /// one segment and `**` matching the rest.
    const ROUTES: &[(&str, &[&str])] = &[
        ("GET", &["v1", "watches"]),
        ("POST", &["v1", "watches"]),
        ("DELETE", &["v1", "watches", "*", "**"]),
        ("GET", &["v1", "notifications"]),
        ("POST", &["v1", "notifications"]),
        ("GET", &["v1", "tasks"]),
        ("POST", &["v1", "tasks"]),
        ("DELETE", &["v1", "tasks", "*"]),
    ];
    // The routes are daemon-side (`/v1/...`), but a daemon whose auth
    // middleware runs inside its `/v1` nest sees the stripped path —
    // accept both spellings, one allowlist.
    let prefixed = format!("/v1{path}");
    let path = if path.starts_with("/v1") {
        path
    } else {
        prefixed.as_str()
    };
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    ROUTES
        .iter()
        .any(|(route_method, pattern)| *route_method == method && match_pattern(pattern, &segments))
}

/// Matches one allowlist pattern against a request's path segments.
fn match_pattern(pattern: &[&str], segments: &[&str]) -> bool {
    match (pattern.first(), segments.first()) {
        (None, None) => true,
        (Some(&"**"), _) => true,
        (Some(p), Some(s)) if *p == "*" || p == s => match_pattern(&pattern[1..], &segments[1..]),
        _ => false,
    }
}

/// The body of a `POST /v1/notifications` push. The device picks which
/// registered device polls the row; `None` means the default device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPush {
    /// The notification's title line.
    pub title: String,
    /// The notification's body text.
    pub body: String,
    /// The device to deliver to; the daemon's default device when
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// One notification row as the daemon returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationOut {
    /// The row id.
    pub id: i64,
    /// The device the row waits for.
    pub device: String,
    /// The title line.
    pub title: String,
    /// The body text.
    pub body: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
    /// Delivery time, milliseconds since the Unix epoch, once the
    /// device acked; `None` while outstanding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
}

/// The body of a `POST /v1/tasks` schedule. `kind` picks the shape:
/// `at` requires `at_millis`, `every` requires `interval_millis`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAdd {
    /// `at` for a one-shot, `every` for a recurring task.
    pub kind: String,
    /// The prompt the firing turn runs.
    pub prompt: String,
    /// The one-shot's firing time, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_millis: Option<i64>,
    /// The recurring task's interval, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_millis: Option<i64>,
    /// The session a firing wakes; the task's own `task:<id>` slot when
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_to: Option<String>,
}

/// One task row as the daemon returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOut {
    /// The row id; `DELETE /v1/tasks/{id}` addresses it.
    pub id: i64,
    /// The prompt the scheduled turn runs.
    pub prompt: String,
    /// `at` or `every`.
    pub kind: String,
    /// The one-shot's firing time, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_millis: Option<i64>,
    /// The recurring task's interval, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_millis: Option<i64>,
    /// When the task next fires, milliseconds since the Unix epoch.
    pub next_due_millis: i64,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at: i64,
    /// The session a firing wakes; `None` meaning the task's own slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_to: Option<String>,
}

/// The removal answer both watches and tasks speak: `{"removed": true}`
/// even when the row was already gone — a replay after a crash
/// converges instead of 404ing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// Whether a row was removed by this call.
    pub removed: bool,
}

/// The one error shape every failure speaks: the last line of stdout is
/// `{"error": "..."}` and the exit code says the class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// What went wrong, in one line a model can react to.
    pub error: String,
}

/// Which bearer a conformance call presents. The transport resolves a
/// class to the actual token it holds; the suite never spells tokens
/// into request bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No `Authorization` header at all.
    None,
    /// The human token: the full surface.
    Human,
    /// The agent token: the [`agent_allowed`] routes only.
    Agent,
    /// A bearer that matches neither configured token.
    Invalid,
}

/// One conformance request.
#[derive(Debug, Clone)]
pub struct CallRequest {
    /// The HTTP method.
    pub method: &'static str,
    /// The daemon-side path, e.g. `/v1/tasks`.
    pub path: String,
    /// Which bearer to present.
    pub auth: Auth,
    /// The `x-rhema-contract` value; `None` sends no header.
    pub contract: Option<u32>,
    /// The JSON body, when the route takes one.
    pub body: Option<serde_json::Value>,
}

/// One conformance response.
#[derive(Debug, Clone)]
pub struct CallResponse {
    /// The HTTP status.
    pub status: u16,
    /// The JSON body; `{}` when the route answers empty.
    pub body: serde_json::Value,
}

mod conformance;
pub use conformance::conformance;

mod mock;
pub use mock::MockClaw;

/// Milliseconds since the Unix epoch — the timestamp vocabulary every
/// contract shape speaks.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
