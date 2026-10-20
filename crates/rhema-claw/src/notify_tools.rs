//! The agent-side notify tool: how the resident reaches a device that is
//! not the live chat surface — "remind me when CI lands" becomes a tool
//! call, the row lands in the [`NotificationStore`], and the device's
//! next poll carries it. No restart, no second mechanism.
//!
//! The spec lives in a `TOOL.yaml` (one folder under `tools/`); the
//! code here is the operation half only — a [`ToolOp`] behind a
//! [`ToolRegistry`], offered through the same allowlist-filtered seam
//! as everything else the model may call (spec §8). Answers are short
//! JSON; failures are in-band strings the model can react to.

use std::collections::BTreeMap;
use std::sync::Arc;

use rhema_autonomous_agent::{ToolOp, ToolRegistry, async_trait};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::notifications::{DEFAULT_DEVICE, NotificationStore};

/// The compiled operation behind the manifest, by `run.builtin` id.
fn builtins(store: Arc<NotificationStore>) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([(
        "notify.push".to_owned(),
        Arc::new(Notify { store }) as Arc<dyn ToolOp>,
    )])
}

/// The notify tool's `TOOL.yaml`, embedded so the daemon ships it with
/// no filesystem layout.
pub const MANIFESTS: &[(&str, &str)] = &[("notify", include_str!("../tools/notify/TOOL.yaml"))];

/// The notification tool set over `store`: `notify` — its spec from
/// the embedded manifest, the operation compiled here.
pub fn registry(store: Arc<NotificationStore>) -> ToolRegistry {
    let (tools, skipped) =
        rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store));
    assert!(
        skipped.is_empty(),
        "the notify tool manifest must load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
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
impl ToolOp for Notify {
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

    #[test]
    fn the_manifest_loads_against_its_builtin_and_serves_the_spec() {
        let (tools, skipped) =
            rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store()));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "notify");
        let resolver =
            registry(store()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("notify"),
            rhema_core::ReplaySafety::Unsafe,
            "re-running a notify call on resume would re-notify"
        );
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
