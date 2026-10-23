//! The mock router: the contract's semantics, in memory, no HTTP. The
//! conformance suite passes against it first — a daemon that fails the
//! suite diverges from this file, and that is the diagnosis.

use std::sync::Mutex;

use serde_json::{Value, json};

use std::sync::PoisonError;

use crate::{
    Auth, CONTRACT_VERSION, CallRequest, CallResponse, NotificationOut, NotificationPush, TaskAdd,
    TaskOut, agent_allowed, now_millis,
};

/// An in-memory daemon speaking the contract. Configure it with the
/// two tokens a real daemon boots with; the conformance suite assumes
/// both are set.
pub struct MockClaw {
    human_token: Option<String>,
    agent_token: Option<String>,
    notifications: Mutex<Vec<NotificationOut>>,
    tasks: Mutex<Vec<TaskOut>>,
    watches: Mutex<Vec<Value>>,
    notification_ids: Mutex<i64>,
    task_ids: Mutex<i64>,
}

impl MockClaw {
    /// A mock with the daemon's two tokens; `None` means that class is
    /// unconfigured and its bearers are strangers.
    pub fn new(human_token: Option<&str>, agent_token: Option<&str>) -> Self {
        Self {
            human_token: human_token.map(str::to_owned),
            agent_token: agent_token.map(str::to_owned),
            notifications: Mutex::new(Vec::new()),
            tasks: Mutex::new(Vec::new()),
            watches: Mutex::new(Vec::new()),
            notification_ids: Mutex::new(0),
            task_ids: Mutex::new(0),
        }
    }

    /// Answers one request, exactly the daemon's auth middleware then
    /// its route handlers would.
    pub async fn call(&self, request: CallRequest) -> Result<CallResponse, String> {
        // The bearer gate. With no tokens configured the surface is
        // open; with either, a stranger is a 401.
        let open = self.human_token.is_none() && self.agent_token.is_none();
        if !open && matches!(request.auth, Auth::None | Auth::Invalid) {
            return Ok(self.error(401, "unauthorized"));
        }
        // The agent class: scoped routes, contract version required.
        if request.auth == Auth::Agent {
            if request.contract != Some(CONTRACT_VERSION) {
                return Ok(self.error(409, "contract version mismatch"));
            }
            if !agent_allowed(request.method, &request.path) {
                return Ok(self.error(403, "this route is not the agent token's"));
            }
        }

        let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
        match (request.method, segments.as_slice()) {
            ("GET", ["v1", "watches"]) => Ok(self.answer(200, json!({
                "watches": self.watches.lock().unwrap_or_else(PoisonError::into_inner).clone(),
                "account_watches": [],
            }))),
            ("POST", ["v1", "watches"]) => {
                let Ok(body) = serde_json::from_value::<WatchAdd>(
                    request.body.clone().unwrap_or(Value::Null),
                ) else {
                    return Ok(self.error(400, "malformed watch body"));
                };
                if !matches!(body.forge.as_str(), "github" | "gitea" | "gitlab")
                    || body.repo.trim().is_empty()
                {
                    return Ok(self.error(400, "forge must be github, gitea, or gitlab"));
                }
                let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
                watches.retain(|w| w["repo"] != body.repo);
                watches.push(json!({"forge": body.forge, "repo": body.repo}));
                Ok(self.answer(200, json!({"forge": body.forge, "repo": body.repo})))
            }
            ("DELETE", ["v1", "watches", forge, rest @ ..]) => {
                let repo = rest.join("/");
                let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
                let before = watches.len();
                watches.retain(|w| w["forge"] != *forge || w["repo"] != repo);
                Ok(self.answer(200, json!({"removed": watches.len() != before})))
            }

            ("GET", ["v1", "notifications"]) => Ok(self.answer(200, json!({
                "notifications": self.notifications.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            }))),
            ("POST", ["v1", "notifications"]) => {
                let Ok(push) = serde_json::from_value::<NotificationPush>(
                    request.body.clone().unwrap_or(Value::Null),
                ) else {
                    return Ok(self.error(400, "malformed notification body"));
                };
                if push.title.trim().is_empty() || push.body.trim().is_empty() {
                    return Ok(self.error(400, "title and body must not be empty"));
                }
                let mut ids = self.notification_ids.lock().unwrap_or_else(PoisonError::into_inner);
                *ids += 1;
                let row = NotificationOut {
                    id: *ids,
                    device: push.device.unwrap_or_else(|| "phone".to_owned()),
                    title: push.title,
                    body: push.body,
                    created_at: now_millis(),
                    delivered_at: None,
                };
                self.notifications
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(row.clone());
                Ok(self.answer(200, serde_json::to_value(row).unwrap_or(Value::Null)))
            }

            ("GET", ["v1", "tasks"]) => Ok(self.answer(200, json!({
                "tasks": self.tasks.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            }))),
            ("POST", ["v1", "tasks"]) => {
                let Ok(add) = serde_json::from_value::<TaskAdd>(
                    request.body.clone().unwrap_or(Value::Null),
                ) else {
                    return Ok(self.error(400, "malformed task body"));
                };
                let now = now_millis();
                let (at_millis, interval_millis, next_due) = match add.kind.as_str() {
                    "at" => {
                        let Some(at) = add.at_millis else {
                            return Ok(self.error(400, "a one-shot task needs at_millis"));
                        };
                        (Some(at), None, at)
                    }
                    "every" => {
                        let Some(interval) = add.interval_millis else {
                            return Ok(self.error(400, "a recurring task needs interval_millis"));
                        };
                        (None, Some(interval), now + interval)
                    }
                    _ => return Ok(self.error(400, "kind must be at or every")),
                };
                let mut ids = self.task_ids.lock().unwrap_or_else(PoisonError::into_inner);
                *ids += 1;
                let row = TaskOut {
                    id: *ids,
                    prompt: add.prompt,
                    kind: add.kind,
                    at_millis,
                    interval_millis,
                    next_due_millis: next_due,
                    created_at: now,
                    deliver_to: add.deliver_to,
                };
                self.tasks.lock().unwrap_or_else(PoisonError::into_inner).push(row.clone());
                Ok(self.answer(200, serde_json::to_value(row).unwrap_or(Value::Null)))
            }
            ("DELETE", ["v1", "tasks", id]) => {
                let Ok(id) = id.parse::<i64>() else {
                    return Ok(self.error(400, "the task id is a number"));
                };
                let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
                let before = tasks.len();
                tasks.retain(|t| t.id != id);
                Ok(self.answer(200, json!({"removed": tasks.len() != before})))
            }

            // Human-surface stand-ins: just enough for the suite's
            // full-surface checks to have something to hit.
            ("GET", ["v1", "consents"]) => Ok(self.answer(200, json!({"consents": []}))),
            ("POST", ["v1", "ingest"]) => {
                let body = request.body.unwrap_or(Value::Null);
                let source_ok = body.get("source").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                let key_ok = body.get("key").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                let payload_ok = body.get("payload").is_some_and(Value::is_object);
                if source_ok && key_ok && payload_ok {
                    Ok(self.answer(200, json!({"accepted": true})))
                } else {
                    Ok(self.error(400, "ingest needs source, key, and a payload object"))
                }
            }
            ("POST", ["v1", "sessions"]) => Ok(self.answer(200, json!({}))),

            _ => Ok(self.error(404, "no such route")),
        }
    }

    /// A JSON answer.
    fn answer(&self, status: u16, body: Value) -> CallResponse {
        CallResponse { status, body }
    }

    /// The one error shape.
    fn error(&self, status: u16, message: &str) -> CallResponse {
        CallResponse {
            status,
            body: json!({"error": message}),
        }
    }
}

/// The body `POST /v1/watches` takes.
#[derive(serde::Deserialize)]
struct WatchAdd {
    /// The forge: `github`, `gitea`, or `gitlab`.
    forge: String,
    /// The `owner/name` repository.
    repo: String,
}
