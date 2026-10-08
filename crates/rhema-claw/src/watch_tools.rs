//! The agent-side watch tools: how the resident agent reconfigures its
//! own watch list mid-conversation — "watch this repo too" becomes a
//! tool call, the row lands in the [`ClawStore`], and the next detect
//! pass picks it up. No restart, no second mechanism.
//!
//! These are [`Tool`] implementations for a [`ToolRegistry`], so the
//! interactive handler offers them through the same allowlist-filtered
//! seam as everything else the model may call (spec §8: the allowlist is
//! fixed before the first step, never decided mid-run). Answers are
//! short JSON; failures are in-band strings the model can react to.
//! Tokens never appear in an answer.

use std::sync::Arc;

use rhema_autonomous_agent::{Tool, ToolRegistry, async_trait};
use rhema_core::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::ForgeKind;
use crate::watch_store::{ClawStore, validate_watch};

/// The watch-management tool set over `store`: `watch_repo`,
/// `unwatch_repo`, `list_watches`.
pub fn registry(store: Arc<ClawStore>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WatchRepo {
        store: store.clone(),
    }));
    registry.register(Arc::new(UnwatchRepo {
        store: store.clone(),
    }));
    registry.register(Arc::new(ListWatches { store }));
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
impl Tool for WatchRepo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "watch_repo",
            "Start watching a repository: new activity (issues, pull \
             requests, comments, reviews) arrives as its own dispatch. \
             `forge` is `github`, `gitea`, or `gitlab`; `repo` is \
             `owner/name` (GitLab: `namespace/project`); gitea also needs \
             `host`, its instance base URL. `token` is optional and falls \
             back to the forge's environment variable.",
            json!({
                "type": "object",
                "properties": {
                    "forge": { "type": "string",
                        "description": "github, gitea, or gitlab." },
                    "repo": { "type": "string",
                        "description": "The repository as owner/name." },
                    "host": { "type": "string",
                        "description": "Instance base URL; required for gitea." },
                    "token": { "type": "string",
                        "description": "Forge token; optional, env fallback." }
                },
                "required": ["forge", "repo"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: WatchArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = ForgeKind::parse(&args.forge)
            .ok_or_else(|| format!("unknown forge {:?}: github, gitea, or gitlab", args.forge))?;
        let host = args.host.as_deref().map(str::trim);
        validate_watch(kind, &args.repo, host)?;
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
impl Tool for UnwatchRepo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "unwatch_repo",
            "Stop watching a repository: no more activity arrives from \
             it. Same `forge` and `repo` spelling as `watch_repo`.",
            json!({
                "type": "object",
                "properties": {
                    "forge": { "type": "string",
                        "description": "github, gitea, or gitlab." },
                    "repo": { "type": "string",
                        "description": "The repository as owner/name." }
                },
                "required": ["forge", "repo"]
            }),
        )
    }

    async fn invoke(&self, args: Value) -> Result<String, String> {
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
impl Tool for ListWatches {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_watches",
            "List the watched repositories: forge, repo, and host for \
             each.",
            json!({
                "type": "object",
                "properties": {},
            }),
        )
    }

    async fn invoke(&self, _args: Value) -> Result<String, String> {
        let watches = self.store.list().map_err(|err| err.to_string())?;
        let rows: Vec<Value> = watches
            .iter()
            .map(|watch| {
                json!({
                    "forge": watch.forge,
                    "repo": watch.repo,
                    "host": watch.host,
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
                {"forge": "github", "repo": "acme/widgets", "host": null},
                {"forge": "gitlab", "repo": "group/prop", "host": null},
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
