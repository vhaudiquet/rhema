//! The agent-side task tools: how the resident schedules its own future
//! turns — "remind me at 15:00" becomes a `schedule_task` call, the row
//! lands in the [`TaskStore`], and the next detect pass of the
//! [`TaskScheduler`](crate::task_source::TaskScheduler) picks it up. No
//! restart, no second mechanism, and no timer anywhere: a due task is
//! an event (spec §1), never a heartbeat.
//!
//! These are [`Tool`] implementations for a [`ToolRegistry`], so the
//! interactive handler offers them through the same allowlist-filtered
//! seam as everything else the model may call (spec §8). Answers are
//! short JSON; failures are in-band strings the model can react to.

use std::sync::Arc;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::task_store::TaskStore;

/// The task tool set over `store`: `schedule_task`, `list_tasks`,
/// `cancel_task`.
pub fn registry(store: Arc<TaskStore>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ScheduleTask {
        store: store.clone(),
    }));
    registry.register(Arc::new(ListTasks {
        store: store.clone(),
    }));
    registry.register(Arc::new(CancelTask { store }));
    registry
}

#[derive(Deserialize)]
struct ScheduleArgs {
    prompt: String,
    #[serde(default)]
    at: Option<Value>,
    #[serde(default)]
    every_secs: Option<u64>,
}

#[derive(Deserialize)]
struct CancelArgs {
    id: i64,
}

/// Milliseconds since the Unix epoch, the table's clock everywhere.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Parses the `at` argument: epoch milliseconds as a JSON number, or a
/// string holding either epoch milliseconds or an RFC3339 timestamp.
fn parse_at_millis(value: Value) -> Result<i64, String> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .ok_or_else(|| "`at` millis are out of range".to_owned()),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
                trimmed
                    .parse::<i64>()
                    .map_err(|_| "`at` millis are out of range".to_owned())
            } else {
                let at = OffsetDateTime::parse(trimmed, &Rfc3339)
                    .map_err(|err| format!("`at` is not an RFC3339 timestamp: {err}"))?;
                Ok(at.unix_timestamp() * 1000 + i64::from(at.millisecond()))
            }
        }
        _ => Err("`at` is an RFC3339 timestamp or epoch milliseconds".to_owned()),
    }
}

/// Schedules one prompt: fire once at `at` (an RFC3339 timestamp or
/// epoch milliseconds) or repeatedly every `every_secs` seconds —
/// exactly one of the two.
struct ScheduleTask {
    store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for ScheduleTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "schedule_task",
            "Schedule a future turn of yours. `prompt` runs as this \
             agent's own turn — once at `at`, or repeatedly every \
             `every_secs` seconds; set exactly one of the two. The \
             scheduled turn has no chat surface: its reply reaches the \
             user as a notification, so write prompts that produce one. \
             Use list_tasks and cancel_task to manage what is scheduled.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string",
                        "description": "What the scheduled turn should do." },
                    "at": { "type": ["string", "number"],
                        "description": "When to fire once: an RFC3339 \
                            timestamp (\"2026-10-10T15:00:00Z\") or epoch \
                            milliseconds." },
                    "every_secs": { "type": "integer",
                        "description": "Repeat every this many seconds." }
                },
                "required": ["prompt"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: ScheduleArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err("prompt must not be empty".to_owned());
        }
        let record = match (args.at, args.every_secs) {
            (Some(at), None) => {
                let at_millis = parse_at_millis(at)?;
                self.store
                    .schedule_at(prompt, at_millis, now_millis())
                    .map_err(|err| err.to_string())?
            }
            (None, Some(every_secs)) => {
                if every_secs == 0 {
                    return Err("every_secs must be at least 1".to_owned());
                }
                let interval = every_secs
                    .checked_mul(1000)
                    .filter(|millis| *millis <= i64::MAX as u64)
                    .ok_or_else(|| "`every_secs` is out of range".to_owned())?;
                self.store
                    .schedule_every(prompt, interval as i64, now_millis())
                    .map_err(|err| err.to_string())?
            }
            (Some(_), Some(_)) => {
                return Err("set exactly one of `at` or `every_secs`, not both".to_owned());
            }
            (None, None) => {
                return Err("set one of `at` or `every_secs`".to_owned());
            }
        };
        Ok(json!({
            "scheduled": true,
            "id": record.id,
            "kind": record.kind.name(),
            "next_due_millis": record.next_due_millis,
        })
        .to_string())
    }
}

/// Lists what is scheduled.
struct ListTasks {
    store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for ListTasks {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_tasks",
            "List the scheduled tasks: id, prompt, kind (at or every), \
             and next_due_millis — when each one fires next.",
            json!({
                "type": "object",
                "properties": {},
            }),
        )
    }

    async fn invoke(&self, _args: Value) -> Result<String, String> {
        let tasks = self.store.list().map_err(|err| err.to_string())?;
        Ok(json!({ "tasks": tasks }).to_string())
    }
}

/// Cancels one task by id.
struct CancelTask {
    store: Arc<TaskStore>,
}

#[async_trait]
impl Tool for CancelTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "cancel_task",
            "Cancel a scheduled task by id (as list_tasks reports it). \
             Already-fired one-shots report cancelled: false.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer",
                        "description": "The task's id." }
                },
                "required": ["id"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: CancelArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let removed = self.store.remove(args.id).map_err(|err| err.to_string())?;
        Ok(json!({ "cancelled": removed, "id": args.id }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::task_store::TaskKind;

    fn store() -> Arc<TaskStore> {
        Arc::new(TaskStore::in_memory().expect("store opens"))
    }

    async fn call(store: &Arc<TaskStore>, name: &str, args: Value) -> rhema_core::ToolResult {
        let resolver =
            registry(store.clone()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        resolver
            .resolve(
                rhema_core::ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    args,
                    provider_data: Value::Null,
                },
                &rhema_core::ToolContext::default(),
            )
            .await
    }

    #[tokio::test]
    async fn schedule_at_lands_a_one_shot_row() {
        let store = store();
        let result = call(
            &store,
            "schedule_task",
            json!({"prompt": "remind the user: bread", "at": 2_000_000_000_000_i64}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap(),
            json!({"scheduled": true, "id": 1, "kind": "at", "next_due_millis": 2_000_000_000_000_i64}),
            "{}",
            result.content
        );
        let listed = store.list().expect("list reads");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, TaskKind::At);
        assert_eq!(listed[0].prompt, "remind the user: bread");
    }

    #[tokio::test]
    async fn an_rfc3339_at_parses_to_millis() {
        let store = store();
        let result = call(
            &store,
            "schedule_task",
            json!({"prompt": "standup", "at": "2026-10-10T15:00:00Z"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let listed = store.list().expect("list reads");
        assert_eq!(listed[0].kind, TaskKind::At);
        assert_eq!(listed[0].next_due_millis, 1_791_644_400_000, "15:00Z");
    }

    #[tokio::test]
    async fn schedule_every_lands_a_recurring_row() {
        let store = store();
        let result = call(
            &store,
            "schedule_task",
            json!({"prompt": "stretch", "every_secs": 1800}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let listed = store.list().expect("list reads");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, TaskKind::Every);
        assert_eq!(listed[0].interval_millis, Some(1_800_000));
        assert!(
            listed[0].next_due_millis > now_millis(),
            "the first firing is in the future"
        );
    }

    #[tokio::test]
    async fn bad_shapes_fail_in_band_and_land_nothing() {
        let store = store();
        for (args, reason) in [
            (
                json!({"prompt": "x", "at": 1, "every_secs": 1}),
                "exactly one",
            ),
            (json!({"prompt": "x"}), "one of"),
            (json!({"prompt": " ", "at": 1}), "empty"),
            (json!({"prompt": "x", "at": "not a time"}), "RFC3339"),
            (json!({"prompt": "x", "at": [1]}), "epoch milliseconds"),
            (json!({"prompt": "x", "every_secs": 0}), "at least 1"),
        ] {
            let result = call(&store, "schedule_task", args).await;
            assert!(result.is_error, "{}", result.content);
            assert!(
                result.content.contains(reason),
                "{reason} in {}",
                result.content
            );
        }
        assert!(
            store.list().expect("list reads").is_empty(),
            "nothing landed"
        );
    }

    #[tokio::test]
    async fn list_and_cancel_round_trip_through_the_table() {
        let store = store();
        store.schedule_at("one", 42, 0).expect("seed");
        store.schedule_every("two", 1_000, 0).expect("seed");

        let listed = call(&store, "list_tasks", json!({})).await;
        assert!(!listed.is_error, "{}", listed.content);
        let value: Value = serde_json::from_str(&listed.content).unwrap();
        assert_eq!(value["tasks"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["tasks"][0]["kind"], "at");
        assert_eq!(value["tasks"][1]["kind"], "every");

        let cancelled = call(&store, "cancel_task", json!({"id": 1})).await;
        assert!(!cancelled.is_error, "{}", cancelled.content);
        assert_eq!(
            serde_json::from_str::<Value>(&cancelled.content).unwrap(),
            json!({"cancelled": true, "id": 1})
        );
        let already_gone = call(&store, "cancel_task", json!({"id": 1})).await;
        assert!(!already_gone.is_error, "{}", already_gone.content);
        assert_eq!(
            serde_json::from_str::<Value>(&already_gone.content).unwrap(),
            json!({"cancelled": false, "id": 1})
        );
        assert_eq!(store.list().expect("list reads").len(), 1);
    }
}
