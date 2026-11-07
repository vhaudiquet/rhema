//! The agent-side watch tools: how the resident agent reconfigures its
//! own watch list mid-conversation — "watch this repo too" becomes a
//! tool call, the row lands in the [`ClawStore`], and the next detect
//! pass picks it up. No restart, no second mechanism.
//!
//! Each tool's spec lives in a `TOOL.yaml` (one folder under
//! `tools/`); the code here is the operation half only — [`ToolOp`]
//! implementations behind a [`ToolRegistry`], offered through the same
//! allowlist-filtered seam as everything else the model may call
//! (spec §8: the allowlist is fixed before the first step, never
//! decided mid-run). Answers are short JSON; failures are in-band
//! strings the model can react to. Tokens never appear in an answer.

use std::collections::BTreeMap;
use std::sync::Arc;

use rhema_autonomous_agent::{ToolError, ToolOp, ToolRegistry, async_trait};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::watch_store::{ClawStore, SCOPE_REPO, validate_watch};
use rhema_forge_integration::ForgeKind;

/// The compiled operations behind the manifests, by `run.builtin` id.
fn builtins(store: Arc<ClawStore>) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([
        (
            "watch.repo".to_owned(),
            Arc::new(WatchRepo {
                store: store.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "watch.unwatch".to_owned(),
            Arc::new(UnwatchRepo {
                store: store.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "watch.list".to_owned(),
            Arc::new(ListWatches { store }) as Arc<dyn ToolOp>,
        ),
    ])
}

/// Every watch tool's `TOOL.yaml`, embedded so the daemon ships them
/// with no filesystem layout: one folder per tool, named by tool, in
/// the order the registry serves them.
pub const MANIFESTS: &[(&str, &str)] = &[
    (
        "list_watches",
        include_str!("../tools/list_watches/TOOL.yaml"),
    ),
    (
        "unwatch_repo",
        include_str!("../tools/unwatch_repo/TOOL.yaml"),
    ),
    ("watch_repo", include_str!("../tools/watch_repo/TOOL.yaml")),
];

/// The watch-management tool set over `store`: `watch_repo`,
/// `unwatch_repo`, `list_watches` — specs from the embedded manifests,
/// operations compiled here.
pub fn registry(store: Arc<ClawStore>) -> ToolRegistry {
    let (tools, skipped) =
        rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store));
    assert!(
        skipped.is_empty(),
        "the watch tool manifests must all load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

#[derive(Deserialize)]
struct WatchArgs {
    forge: String,
    repo: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
struct UnwatchArgs {
    forge: String,
    repo: String,
}

/// Starts watching: one row in the table.
struct WatchRepo {
    store: Arc<ClawStore>,
}

#[async_trait]
impl ToolOp for WatchRepo {
    async fn invoke(&self, args: Value) -> Result<String, ToolError> {
        let args: WatchArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = ForgeKind::parse(&args.forge)
            .ok_or_else(|| format!("unknown forge {:?}: github, gitea, or gitlab", args.forge))?;
        let host = args.host.as_deref().map(str::trim);
        // The tool contract answers in band: the model sees the shape
        // refusal as text it can correct.
        validate_watch(kind, &args.repo, host).map_err(|err| err.to_string())?;
        let added = self
            .store
            .add(
                kind.name(),
                args.repo.trim(),
                host,
                args.token.as_deref().map(str::trim).unwrap_or(""),
            )
            .map_err(|err| err.to_string())?;
        Ok(json!({
            "watched": true,
            "added": added,
            "forge": kind.name(),
            "repo": args.repo.trim(),
        })
        .to_string())
    }
}

/// Stops watching: the row leaves the table.
struct UnwatchRepo {
    store: Arc<ClawStore>,
}

#[async_trait]
impl ToolOp for UnwatchRepo {
    async fn invoke(&self, args: Value) -> Result<String, ToolError> {
        let args: UnwatchArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = ForgeKind::parse(&args.forge)
            .ok_or_else(|| format!("unknown forge {:?}: github, gitea, or gitlab", args.forge))?;
        let removed = self
            .store
            .remove(kind.name(), args.repo.trim())
            .map_err(|err| err.to_string())?;
        Ok(json!({
            "watched": false,
            "removed": removed,
            "forge": kind.name(),
            "repo": args.repo.trim(),
        })
        .to_string())
    }
}

/// Lists what is watched.
struct ListWatches {
    store: Arc<ClawStore>,
}

#[async_trait]
impl ToolOp for ListWatches {
    async fn invoke(&self, _args: Value) -> Result<String, ToolError> {
        let watches = self.store.list().map_err(|err| err.to_string())?;
        let health = self.store.list_health().map_err(|err| err.to_string())?;
        let rows: Vec<Value> = watches
            .iter()
            .map(|watch| {
                // A watch that has never polled has no health row at all:
                // it answers with every health field empty and a zero
                // streak, which is exactly what it is.
                let health = health.iter().find(|row| {
                    row.scope == SCOPE_REPO && row.forge == watch.forge && row.subject == watch.repo
                });
                json!({
                    "forge": watch.forge,
                    "repo": watch.repo,
                    "host": watch.host,
                    "last_ok_at": health.and_then(|row| row.last_ok_at),
                    "last_error": health.and_then(|row| row.last_error.clone()),
                    "consecutive_failures": health
                        .map(|row| row.consecutive_failures)
                        .unwrap_or(0),
                })
            })
            .collect();
        Ok(json!({ "watches": rows }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<ClawStore> {
        Arc::new(ClawStore::in_memory().expect("store opens"))
    }

    fn names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .resolver_for(&rhema_autonomous_agent::Allowlist::permissive())
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    #[test]
    fn every_manifest_loads_against_its_builtin() {
        let (tools, skipped) =
            rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(store()));
        assert!(skipped.is_empty(), "{skipped:?}");
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert_eq!(
            names,
            ["list_watches", "unwatch_repo", "watch_repo"].map(str::to_owned)
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
    fn replay_safety_comes_from_the_manifest() {
        let resolver =
            registry(store()).resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("list_watches"),
            rhema_core::ReplaySafety::Safe
        );
        assert_eq!(
            resolver.replay_safety("watch_repo"),
            rhema_core::ReplaySafety::Unsafe,
            "re-running a watch call on resume could double-watch"
        );
    }

    #[tokio::test]
    async fn the_registry_offers_the_three_tools() {
        let tools = registry(store());
        assert_eq!(
            names(&tools),
            ["list_watches", "unwatch_repo", "watch_repo"].map(str::to_owned),
            "BTreeMap order: the registry sorts by name"
        );
    }

    #[tokio::test]
    async fn watch_repo_lands_a_row_and_answers_json() {
        let store = store();
        let tools = registry(store.clone());
        let resolver = tools.resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        let ctx = rhema_core::ToolContext::default();
        let result = resolver
            .resolve(
                rhema_core::ToolCall {
                    id: "c1".into(),
                    name: "watch_repo".into(),
                    args: json!({
                        "forge": "github",
                        "repo": "acme/widgets",
                        "token": "sekrit",
                    }),
                    provider_data: Value::Null,
                },
                &ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap(),
            json!({"watched": true, "added": true, "forge": "github", "repo": "acme/widgets"}),
            "{}",
            result.content
        );
        let watches = store.list().expect("list reads");
        assert_eq!(watches[0].repo, "acme/widgets");
        assert_eq!(
            watches[0].token, "sekrit",
            "the token is stored, not echoed"
        );
        assert!(
            !result.content.contains("sekrit"),
            "answers never carry tokens"
        );
    }

    #[tokio::test]
    async fn a_bad_watch_fails_in_band_with_the_reason() {
        let store = store();
        let tools = registry(store.clone());
        let resolver = tools.resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        let ctx = rhema_core::ToolContext::default();
        let call = |name: &str, args: Value| {
            resolver.resolve(
                rhema_core::ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    args,
                    provider_data: Value::Null,
                },
                &ctx,
            )
        };
        let hostless = call("watch_repo", json!({"forge": "gitea", "repo": "acme/barn"})).await;
        assert!(hostless.is_error);
        assert!(
            hostless.content.contains("self-hosted"),
            "{}",
            hostless.content
        );

        let unknown = call(
            "watch_repo",
            json!({"forge": "sourcehut", "repo": "acme/barn"}),
        )
        .await;
        assert!(unknown.is_error);
        assert!(
            unknown.content.contains("unknown forge"),
            "{}",
            unknown.content
        );

        let malformed = call("watch_repo", json!({"forge": "github"})).await;
        assert!(malformed.is_error);
        assert!(
            malformed.content.contains("bad arguments"),
            "{}",
            malformed.content
        );

        assert!(
            store.list().expect("list reads").is_empty(),
            "nothing landed"
        );
    }

    #[tokio::test]
    async fn unwatch_and_list_round_trip_through_the_same_table() {
        let store = store();
        store.add("github", "acme/widgets", None, "").expect("seed");
        store.add("gitlab", "group/prop", None, "").expect("seed");
        let tools = registry(store.clone());
        let resolver = tools.resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        let ctx = rhema_core::ToolContext::default();
        let call = |name: &str, args: Value| {
            resolver.resolve(
                rhema_core::ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    args,
                    provider_data: Value::Null,
                },
                &ctx,
            )
        };

        let listed = call("list_watches", json!({})).await;
        assert!(!listed.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&listed.content).unwrap(),
            json!({"watches": [
                {
                    "forge": "github",
                    "repo": "acme/widgets",
                    "host": null,
                    "last_ok_at": null,
                    "last_error": null,
                    "consecutive_failures": 0,
                },
                {
                    "forge": "gitlab",
                    "repo": "group/prop",
                    "host": null,
                    "last_ok_at": null,
                    "last_error": null,
                    "consecutive_failures": 0,
                },
            ]}),
            "{}",
            listed.content
        );

        let removed = call(
            "unwatch_repo",
            json!({"forge": "github", "repo": "acme/widgets"}),
        )
        .await;
        assert!(!removed.is_error, "{}", removed.content);
        assert_eq!(
            serde_json::from_str::<Value>(&removed.content).unwrap(),
            json!({"watched": false, "removed": true, "forge": "github", "repo": "acme/widgets"}),
        );
        assert_eq!(store.list().expect("list reads").len(), 1);
    }
}
