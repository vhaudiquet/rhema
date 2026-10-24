//! The agent-side task tools: how the resident schedules its own future
//! turns — "remind me at 15:00" becomes a `schedule_task` call, the row
//! lands in the [`TaskStore`], and the next detect pass of the
//! [`TaskScheduler`](crate::task_source::TaskScheduler) picks it up. No
//! restart, no second mechanism, and no timer anywhere: a due task is
//! an event (spec §1), never a heartbeat.
//!
//! The spec the model sees lives in a `TOOL.yaml` per tool (one
//! folder under `tools/`, embedded at compile time); the code here is
//! the operation half only — [`ToolOp`] implementations behind a
//! [`ToolRegistry`], offered through the same allowlist-filtered seam
//! as everything else the model may call (spec §8). Answers are short
//! JSON; failures are in-band strings the model can react to.

use std::collections::BTreeMap;
use std::sync::Arc;

use rhema_autonomous_agent::{ToolError, ToolOp, ToolRegistry, async_trait};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::interactive::is_resident_key;
use crate::task_store::TaskStore;

/// The compiled operations behind the manifests, by `run.builtin` id:
/// the id is a manifest's only link to this code.
fn builtins(store: Arc<TaskStore>) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([
        (
            "task.schedule".to_owned(),
            Arc::new(ScheduleTask {
                store: store.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "task.list".to_owned(),
            Arc::new(ListTasks {
                store: store.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "task.cancel".to_owned(),
            Arc::new(CancelTask { store }) as Arc<dyn ToolOp>,
        ),
    ])
}

/// Every task tool's `TOOL.yaml`, embedded so the daemon ships them
/// with no filesystem layout: one folder per tool, named by tool.
pub const MANIFESTS: &[(&str, &str)] = &[
    (
        "schedule_task",
        include_str!("../tools/schedule_task/TOOL.yaml"),
    ),
    ("list_tasks", include_str!("../tools/list_tasks/TOOL.yaml")),
    (
        "cancel_task",
        include_str!("../tools/cancel_task/TOOL.yaml"),
    ),
];

/// The task tool set over `store`: `schedule_task`, `list_tasks`,
/// `cancel_task` — specs from the embedded manifests, operations
/// compiled here.
pub fn registry(store: Arc<TaskStore>) -> ToolRegistry {
    let (tools, skipped) =
        rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store));
    assert!(
        skipped.is_empty(),
        "the task tool manifests must all load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

#[derive(Deserialize)]
struct ScheduleArgs {
    prompt: String,
    #[serde(default)]
    at: Option<Value>,
    #[serde(default)]
    every_secs: Option<u64>,
    #[serde(default)]
    session: Option<String>,
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
impl ToolOp for ScheduleTask {
    async fn invoke(&self, args: Value) -> Result<String, ToolError> {
        let args: ScheduleArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err("prompt must not be empty".to_owned().into());
        }
        // A blank session reads as unset, and only a resident key can be
        // woken: the delivery target must be a session the resident
        // dispatcher answers.
        let deliver_to = args
            .session
            .as_deref()
            .map(str::trim)
            .filter(|session| !session.is_empty());
        if let Some(session) = deliver_to {
            if !is_resident_key(session) {
                return Err(format!(
                    "session must be a resident key (interactive:, task:, or phone:), got {session:?}"
                )
                .into());
            }
        }
        let record = match (args.at, args.every_secs) {
            (Some(at), None) => {
                let at_millis = parse_at_millis(at)?;
                self.store
                    .schedule_at(prompt, at_millis, now_millis(), deliver_to)
                    .map_err(|err| err.to_string())?
            }
            (None, Some(every_secs)) => {
                if every_secs == 0 {
                    return Err("every_secs must be at least 1".to_owned().into());
                }
                let interval = every_secs
                    .checked_mul(1000)
                    .filter(|millis| *millis <= i64::MAX as u64)
                    .ok_or_else(|| "`every_secs` is out of range".to_owned())?;
                self.store
                    .schedule_every(prompt, interval as i64, now_millis(), deliver_to)
                    .map_err(|err| err.to_string())?
            }
            (Some(_), Some(_)) => {
                return Err("set exactly one of `at` or `every_secs`, not both"
                    .to_owned()
                    .into());
            }
            (None, None) => {
                return Err("set one of `at` or `every_secs`".to_owned().into());
            }
        };
        Ok(json!({
            "scheduled": true,
            "id": record.id,
            "kind": record.kind.name(),
            "next_due_millis": record.next_due_millis,
            "session": record.deliver_to,
        })
        .to_string())
    }
}

/// Lists what is scheduled.
struct ListTasks {
    store: Arc<TaskStore>,
}

#[async_trait]
impl ToolOp for ListTasks {
    async fn invoke(&self, _args: Value) -> Result<String, ToolError> {
        let tasks = self.store.list().map_err(|err| err.to_string())?;
        Ok(json!({ "tasks": tasks }).to_string())
    }
}

/// Cancels one task by id.
struct CancelTask {
    store: Arc<TaskStore>,
}

#[async_trait]
impl ToolOp for CancelTask {
    async fn invoke(&self, args: Value) -> Result<String, ToolError> {
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

    #[test]
    fn every_manifest_loads_against_its_builtin() {
        let (tools, skipped) =
            rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store()));
        assert!(skipped.is_empty(), "{skipped:?}");
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert_eq!(
            names,
            vec![
                "schedule_task".to_owned(),
                "list_tasks".to_owned(),
                "cancel_task".to_owned()
            ]
        );
    }

    #[test]
    fn every_builtin_is_named_by_a_manifest() {
        let mut covered = Vec::new();
        for (source, raw) in MANIFESTS {
            covered.push(
                rhema_autonomous_agent::manifest::parse(raw)
                    .unwrap_or_else(|err| panic!("{source}: {err}"))
                    .builtin,
            );
        }
        for id in builtins(store()).keys() {
            assert!(
                covered.iter().any(|builtin| builtin == id),
                "{id} has no TOOL.yaml"
            );
        }
    }

    #[test]
    fn the_registry_serves_the_manifest_specs() {
        let resolver =
            registry(store()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        let specs = resolver.specs();
        for (name, raw) in MANIFESTS {
            let manifest = rhema_autonomous_agent::manifest::parse(raw)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            let spec = specs
                .iter()
                .find(|spec| spec.name == *name)
                .unwrap_or_else(|| panic!("{name} is not offered"));
            assert_eq!(
                spec,
                &rhema_core::ToolSpec::new(
                    manifest.name,
                    manifest.description,
                    manifest.parameters,
                ),
                "{name} serves its manifest"
            );
        }
    }

    #[test]
    fn replay_safety_comes_from_the_manifest() {
        let resolver =
            registry(store()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("list_tasks"),
            rhema_core::ReplaySafety::Safe
        );
        assert_eq!(
            resolver.replay_safety("schedule_task"),
            rhema_core::ReplaySafety::Unsafe,
            "re-running a schedule call on resume could double-schedule"
        );
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
            json!({"scheduled": true, "id": 1, "kind": "at", "next_due_millis": 2_000_000_000_000_i64, "session": null}),
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

    /// The wake-itself shape: a session-named task lands with its
    /// delivery target, and a non-resident target is refused in band.
    #[tokio::test]
    async fn a_session_target_lands_and_a_foreign_one_is_refused() {
        let store = store();
        let result = call(
            &store,
            "schedule_task",
            json!({
                "prompt": "summarize this evening's work",
                "at": "2026-10-15T08:00:00Z",
                "session": "interactive:web",
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let answer: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(answer["session"], "interactive:web");
        assert_eq!(
            store.list().expect("reads")[0].deliver_to.as_deref(),
            Some("interactive:web")
        );

        // A blank session reads as unset — the default task slot.
        for blank in ["", "  "] {
            let accepted = call(
                &store,
                "schedule_task",
                json!({"prompt": "x", "at": 1, "session": blank}),
            )
            .await;
            assert!(!accepted.is_error, "{blank:?} reads as unset");
            assert_eq!(
                store
                    .list()
                    .expect("reads")
                    .last()
                    .expect("lands")
                    .deliver_to,
                None,
                "{blank:?} schedules into the task's own slot"
            );
        }
        // A non-resident target, or a bare name, cannot be woken.
        for foreign in ["github:acme/w#7", "web"] {
            let refused = call(
                &store,
                "schedule_task",
                json!({"prompt": "x", "at": 1, "session": foreign}),
            )
            .await;
            assert!(refused.is_error, "{foreign:?} must be refused");
        }
        assert_eq!(
            store
                .list()
                .expect("reads")
                .iter()
                .filter(|task| task.deliver_to.is_some())
                .count(),
            1,
            "the named session is the only targeted row"
        );
    }

    #[tokio::test]
    async fn list_and_cancel_round_trip_through_the_table() {
        let store = store();
        store.schedule_at("one", 42, 0, None).expect("seed");
        store.schedule_every("two", 1_000, 0, None).expect("seed");

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
