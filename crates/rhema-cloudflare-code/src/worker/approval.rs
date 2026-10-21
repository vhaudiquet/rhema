//! The WebSocket approval channel: the async round-trip workerd needs.
//!
//! The gate ([`crate::gate::ClientGate`]) consults this channel at an
//! await point; the ask goes out on the socket, the answer comes back
//! through the socket's pump, which resolves the matching one-shot by
//! call id. No thread is ever blocked — there is only one thread, and
//! the pump runs on it while this ask waits. A client that never
//! answers is denied at the timeout, via a host-timer delay, never a
//! tokio sleep.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use rhema_core::ToolCall;

use crate::gate::ApprovalChannel;

/// What one live connection shares between the pump and the turn: the
/// socket answers go out on, the pending approvals, and the turn's
/// cancel flag.
pub struct Live {
    /// The server side of the accepted socket. Cloneable; sends are
    /// synchronous host calls.
    pub socket: worker::WebSocket,
    /// Approvals awaiting their client, by call id.
    pub pending: Mutex<HashMap<String, futures::channel::oneshot::Sender<bool>>>,
    /// The current turn's cancel flag; a stop message or a disconnect
    /// flips it. An `Arc` because the turn's context holds the same flag.
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Whether a turn is running; a second chat message is refused.
    pub busy: std::sync::atomic::AtomicBool,
}

impl Live {
    /// Sends one JSON message to the client.
    pub fn send_json(&self, value: &serde_json::Value) {
        let Ok(text) = serde_json::to_string(value) else {
            return;
        };
        let _ = self.socket.send_with_str(text);
    }
}

/// The channel: asks the connected client, denies on timeout.
pub struct WsChannel {
    live: Arc<Live>,
    timeout: Duration,
}

impl WsChannel {
    /// A channel over `live`, denying asks `timeout` old.
    pub fn new(live: Arc<Live>, timeout: Duration) -> Self {
        Self { live, timeout }
    }

    /// Resolves a pending approval by call id — the pump's path for an
    /// `approve` message.
    pub fn answer(&self, call_id: &str, approved: bool) {
        if let Some(sender) = self
            .live
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
        {
            let _ = sender.send(approved);
        }
    }

    /// Drops every pending approval — the disconnect path.
    pub fn abandon_all(&self) {
        self.live
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// The command text of a call, for the prompt the client shows.
fn command_of(call: &ToolCall) -> String {
    call.args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default()
}

#[async_trait]
impl ApprovalChannel for WsChannel {
    async fn ask(&self, call: &ToolCall) -> Option<bool> {
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.live
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call.id.clone(), sender);
        self.live.send_json(&serde_json::json!({
            "type": "approval_request",
            "id": call.id,
            "tool": call.name,
            "command": command_of(call),
        }));
        // The answer arrives through the pump; the timeout arrives
        // through the host's timer. Either way this task suspends and
        // the socket stays live — no thread is held. The timer's
        // `!Send` future runs on the local task queue, handing back a
        // plain one-shot: the JS handle stays inside the bridge.
        let (timed_out, timed_out_rx) = futures::channel::oneshot::channel::<()>();
        let timeout = self.timeout;
        wasm_bindgen_futures::spawn_local(async move {
            worker::Delay::from(timeout).await;
            let _ = timed_out.send(());
        });
        let timeout = futures::future::select(receiver.fuse(), timed_out_rx.fuse()).await;
        self.live
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&call.id);
        match timeout {
            futures::future::Either::Left((Ok(answer), _)) => Some(answer),
            // The client disconnected mid-ask: denied.
            futures::future::Either::Left((Err(_), _)) => None,
            // Timed out: denied, and the client is told.
            futures::future::Either::Right(_) => {
                self.live.send_json(&serde_json::json!({
                    "type": "approval_expired",
                    "id": call.id,
                }));
                None
            }
        }
    }
}
