//! The approval bridge: the sync [`Approval`] hook answered by the
//! client's `session/request_permission`.
//!
//! DEADLOCK CONSTRAINT: [`Approval::approve`] is synchronous and runs on
//! the prompt task inside the ACP connection's runtime, while the client's
//! answer arrives through the SDK's dispatch loop — a separate task. The
//! bridge therefore parks the prompt task with `block_in_place`
//! (`serve` builds a multi-thread runtime; that is the invariant) and
//! awaits the request response through [`tokio::runtime::Handle`], the
//! same shape as the TUI's modal bridge with the runtime standing in for
//! the UI thread.
//!
//! Fail-safety: a failed send (connection gone) or a failed/cancelled
//! response declines. `allow always` and `reject always` remember the
//! exact command for the rest of the session — for `bash`, the command
//! string, mirroring the allowlist semantics a shell tool can honor.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, ToolCallId, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};
use rhema_coding_agent::Approval;
use rhema_core::ToolCall;

/// How long one permission request may sit unanswered before the call
/// declines and the turn moves on. A modal nobody answers otherwise
/// parks the slot's worker forever — `session/cancel` cannot reach a
/// blocked future.
const APPROVAL_BUDGET: Duration = Duration::from_secs(300);

/// What the session remembers from past `always` answers: exact
/// `name:argument` keys, allowed or rejected for the session's life.
#[derive(Default)]
pub(crate) struct AlwaysMemory {
    allowed: HashSet<String>,
    rejected: HashSet<String>,
}

impl AlwaysMemory {
    /// The key an answer is remembered under: tool name plus the one
    /// argument that identifies the action (the command), so "always"
    /// covers repeated identical work, never a different one.
    fn key(call: &ToolCall) -> String {
        let command = call
            .args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        format!("{}:{command}", call.name)
    }
}

/// The turn-side [`Approval`]: forwards the request to the client,
/// parks for the answer, declines on any failure.
pub(crate) struct AcpApproval {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    memory: Arc<Mutex<AlwaysMemory>>,
}

impl AcpApproval {
    /// Pairs a turn-side approval with the session's shared `always`
    /// memory, so answers outlive the turn that made them.
    pub(crate) fn new(
        cx: ConnectionTo<Client>,
        session_id: SessionId,
        memory: Arc<Mutex<AlwaysMemory>>,
    ) -> Self {
        Self {
            cx,
            session_id,
            memory,
        }
    }

    fn memory(&self) -> MutexGuard<'_, AlwaysMemory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn request(&self, call: &ToolCall, key: &str) -> bool {
        let options = vec![
            PermissionOption::new(
                PermissionOptionId::new("allow-always"),
                "Always allow",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(
                PermissionOptionId::new("allow-once"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::new("reject-once"),
                "Reject",
                PermissionOptionKind::RejectOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::new("reject-always"),
                "Always reject",
                PermissionOptionKind::RejectAlways,
            ),
        ];
        let request = RequestPermissionRequest::new(
            self.session_id.clone(),
            ToolCallUpdate::new(
                ToolCallId::new(call.id.clone()),
                ToolCallUpdateFields::new()
                    .title(title(call))
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending),
            ),
            options,
        );
        let response: RequestPermissionResponse =
            match self.cx.send_request(request).block_task().await {
                Ok(response) => response,
                // The connection or the turn is gone: decline so the turn
                // unwinds instead of parking forever.
                // The connection or the turn is gone: decline so the turn
                // unwinds instead of parking forever.
                Err(_) => return false,
            };
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                let option_id = selected.option_id.0.as_ref();
                match option_id {
                    "allow-once" => true,
                    "allow-always" => {
                        self.memory().allowed.insert(key.to_owned());
                        true
                    }
                    "reject-always" => {
                        self.memory().rejected.insert(key.to_owned());
                        false
                    }
                    // "reject-once" and anything unrecognized declines.
                    _ => false,
                }
            }
            // The turn was cancelled while the request was up.
            RequestPermissionOutcome::Cancelled => false,
            // The outcome enum is open; an unknown outcome declines.
            _ => false,
        }
    }
}

impl Approval for AcpApproval {
    fn approve(&self, call: &ToolCall) -> bool {
        let key = AlwaysMemory::key(call);
        {
            let memory = self.memory();
            if memory.allowed.contains(&key) {
                return true;
            }
            if memory.rejected.contains(&key) {
                return false;
            }
        }
        // Parks the prompt task's worker; the answer comes from the
        // SDK's dispatch task, which keeps running. Panics outside a
        // multi-thread runtime — `serve` builds the only runtime this
        // hook ever runs under. The park is bounded: a client that
        // opens the modal and never answers must not wedge the slot —
        // past the budget the call declines, and the turn carries the
        // in-band failure to the model.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::timeout(APPROVAL_BUDGET, self.request(call, &key))
                    .await
                    .unwrap_or_default()
            })
        })
    }
}

/// The human title for a permission request: the command when the call
/// carries one, the tool name otherwise.
fn title(call: &ToolCall) -> String {
    call.args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| call.name.clone())
}
