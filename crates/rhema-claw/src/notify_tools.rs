//! The agent-side notify tool: how the resident reaches a device that is
//! not the live chat surface — "remind me when CI lands" becomes a tool
//! call, the row lands in the [`NotificationStore`], and the device's
//! next poll carries it. No restart, no second mechanism.
//!
//! This is a [`Tool`] implementation for a [`ToolRegistry`], so the
//! interactive handler offers it through the same allowlist-filtered
//! seam as everything else the model may call (spec §8). Answers are
//! short JSON; failures are in-band strings the model can react to.

use std::sync::Arc;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::notifications::{DEFAULT_DEVICE, NotificationStore};

/// The notification tool set over `store`: `notify`.
pub fn registry(store: Arc<NotificationStore>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Notify { store }));
    registry
}

#[derive(Deserialize)]
struct NotifyArgs {
    title: String,
    body: String,
    #[serde(default)]
    device: Option<String>,
}

/// Sends one notification: one row in the table, delivered at the
/// device's next poll-and-ack.
struct Notify {
    store: Arc<NotificationStore>,
}

#[async_trait]
impl Tool for Notify {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "notify",
            "Send a notification to one of the user's devices: the phone \
             by default. Use it to reach the user away from this chat — \
             a reminder they asked to be passed on, finished background \
             work, anything that must not wait for their next message.",
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string",
                        "description": "The one-line headline." },
                    "body": { "type": "string",
                        "description": "The message body." },
                    "device": { "type": "string",
                        "description": "The target device; the phone \
                            when omitted." }
                },
                "required": ["title", "body"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: NotifyArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let title = args.title.trim();
        let body = args.body.trim();
        if title.is_empty() {
            return Err("title must not be empty".to_owned());
        }
        if body.is_empty() {
            return Err("body must not be empty".to_owned());
        }
        let device = args
            .device
            .as_deref()
            .map(str::trim)
            .filter(|device| !device.is_empty())
            .unwrap_or(DEFAULT_DEVICE);
        let record = self
            .store
            .push(device, title, body)
            .map_err(|err| err.to_string())?;
        Ok(json!({
            "queued": true,
            "id": record.id,
            "device": record.device,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<NotificationStore> {
        Arc::new(NotificationStore::in_memory().expect("store opens"))
    }

    async fn call(store: &Arc<NotificationStore>, args: Value) -> rhema_core::ToolResult {
        let resolver =
            registry(store.clone()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        resolver
            .resolve(
                rhema_core::ToolCall {
                    id: "c1".into(),
                    name: "notify".into(),
                    args,
                    provider_data: Value::Null,
                },
                &rhema_core::ToolContext::default(),
            )
            .await
    }

    #[tokio::test]
    async fn notify_enqueues_a_row_and_answers_one_line() {
        let store = store();
        let result = call(
            &store,
            json!({"title": "CI green", "body": "all checks pass"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap(),
            json!({"queued": true, "id": 1, "device": "phone"}),
            "{}",
            result.content
        );
        let pending = store.pending(DEFAULT_DEVICE, 0).expect("poll reads");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "CI green");
        assert_eq!(pending[0].body, "all checks pass");
        assert_eq!(pending[0].device, "phone", "the default device");
    }

    #[tokio::test]
    async fn a_named_device_lands_on_its_own_queue() {
        let store = store();
        let result = call(
            &store,
            json!({"title": "done", "body": "shipped", "device": " desk "}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            store.pending("desk", 0).expect("poll reads").len() == 1
                && store
                    .pending(DEFAULT_DEVICE, 0)
                    .expect("poll reads")
                    .is_empty(),
            "the trimmed device owns the row"
        );
    }

    #[tokio::test]
    async fn empty_fields_and_bad_arguments_fail_in_band() {
        let store = store();
        for args in [
            json!({"title": "  ", "body": "x"}),
            json!({"title": "x", "body": ""}),
            json!({"title": "x"}),
        ] {
            let result = call(&store, args).await;
            assert!(result.is_error, "{}", result.content);
        }
        assert!(
            store
                .pending(DEFAULT_DEVICE, 0)
                .expect("poll reads")
                .is_empty(),
            "nothing landed"
        );
    }
}
