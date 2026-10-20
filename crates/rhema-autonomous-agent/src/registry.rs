//! The tool registry (spec §5's `Tool` contract): named tools aggregated
//! into one allowlist-filtered [`ToolResolver`].
//!
//! `rhema_core` deliberately ships the resolver seam without a registry —
//! one primitive, not a registry. The registry is a resident-agent
//! concern: many named tools, and a per-run permission filter (spec §8)
//! applied before the handler's first step, so the model only ever sees
//! the tools its allowlist permits.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use rhema_core::{ReplaySafety, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};
use serde_json::Value;

use crate::Allowlist;

/// One named tool: the spec the model sees, and the invocation the
/// registry dispatches to. Errors surface in-band — a failing tool is a
/// result the model can react to, never a crashed run.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's schema, passed through to the provider's native
    /// tool-calling format. The description lives here and nowhere else.
    fn spec(&self) -> ToolSpec;

    /// Whether a resume may re-execute one of this tool's committed
    /// calls that lost its result. [`ReplaySafety::Unsafe`] unless
    /// declared otherwise: an unclassified tool is treated as
    /// side-effecting.
    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Unsafe
    }

    /// Runs the tool. The registry turns `Err` into an in-band failure
    /// result; `Ok` text is the tool's answer.
    async fn invoke(&self, args: Value) -> Result<String, String>;
}

/// The named tool set, from which per-run [`ToolResolver`]s are cut.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool under its spec's name; a re-register replaces.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    /// How many tools are registered.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// A resolver over the tools `allowlist` permits: the model sees only
    /// those specs, and a call for a forbidden or unknown tool comes back
    /// as an in-band failure result.
    pub fn resolver_for(&self, allowlist: &Allowlist) -> Arc<dyn ToolResolver> {
        Arc::new(FilteredResolver {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| allowlist.allows(name))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
        })
    }
}

/// One run's view of the registry: the allowlisted slice, as the seam the
/// step engine consumes.
struct FilteredResolver {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

#[async_trait]
impl ToolResolver for FilteredResolver {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    fn replay_safety(&self, name: &str) -> ReplaySafety {
        self.tools
            .get(name)
            .map(|tool| tool.replay_safety())
            .unwrap_or(ReplaySafety::Unsafe)
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        let Some(tool) = self.tools.get(&call.name) else {
            return ToolResult::failure(&call.id, format!("unknown tool: {}", call.name));
        };
        match tool.invoke(call.args.clone()).await {
            Ok(text) => ToolResult::success(&call.id, text),
            Err(text) => ToolResult::failure(&call.id, text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("echo", "Echo the input back", serde_json::json!({}))
        }
        async fn invoke(&self, args: Value) -> Result<String, String> {
            Ok(args.to_string())
        }
    }

    struct Boom;
    #[async_trait]
    impl Tool for Boom {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("boom", "Always fails", serde_json::json!({}))
        }
        async fn invoke(&self, _args: Value) -> Result<String, String> {
            Err("exploded".to_owned())
        }
    }

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Echo));
        registry.register(Arc::new(Boom));
        registry
    }

    #[tokio::test]
    async fn a_filtered_resolver_only_offers_allowed_specs() {
        let resolver = registry().resolver_for(&Allowlist::from_tools(["echo"]));
        let names: Vec<String> = resolver.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["echo".to_owned()]);
    }

    #[tokio::test]
    async fn a_forbidden_tool_is_an_in_band_failure() {
        let resolver = registry().resolver_for(&Allowlist::from_tools(["echo"]));
        let ctx = ToolContext::default();
        let result = resolver
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "boom".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ctx,
            )
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn an_allowed_tool_runs_and_errors_stay_in_band() {
        let resolver = registry().resolver_for(&Allowlist::permissive());
        let ctx = ToolContext::default();
        let ok = resolver
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "echo".into(),
                    args: serde_json::json!({ "x": 1 }),
                    provider_data: serde_json::Value::Null,
                },
                &ctx,
            )
            .await;
        assert!(!ok.is_error);
        let failed = resolver
            .resolve(
                ToolCall {
                    id: "call_2".into(),
                    name: "boom".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ctx,
            )
            .await;
        assert!(failed.is_error);
        assert!(failed.content.contains("exploded"));
    }

    #[tokio::test]
    async fn a_re_register_replaces_the_tool_under_its_name() {
        struct EchoLoud;
        #[async_trait]
        impl Tool for EchoLoud {
            fn spec(&self) -> ToolSpec {
                ToolSpec::new("echo", "Echo, but louder", serde_json::json!({}))
            }
            async fn invoke(&self, args: Value) -> Result<String, String> {
                Ok(format!("LOUD {args}"))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Echo));
        registry.register(Arc::new(EchoLoud));
        assert_eq!(registry.len(), 1, "the name has one occupant");

        let resolver = registry.resolver_for(&Allowlist::permissive());
        let result = resolver
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "echo".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ToolContext::default(),
            )
            .await;
        assert!(!result.is_error);
        assert!(
            result.content.contains("LOUD"),
            "the replacement serves the call"
        );
    }

    #[tokio::test]
    async fn an_empty_registry_offers_no_specs_and_fails_calls_in_band() {
        let resolver = ToolRegistry::new().resolver_for(&Allowlist::permissive());
        assert!(resolver.specs().is_empty());
        let result = resolver
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "anything".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ToolContext::default(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn a_tool_registered_but_filtered_is_indistinguishable_from_unknown() {
        // The model must not learn the forbidden tool exists: a call for it
        // gets the same in-band failure an unknown name gets.
        let filtered = registry().resolver_for(&Allowlist::from_tools(["echo"]));
        let forbidden = filtered
            .resolve(
                ToolCall {
                    id: "call_1".into(),
                    name: "boom".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ToolContext::default(),
            )
            .await;
        let unknown = filtered
            .resolve(
                ToolCall {
                    id: "call_2".into(),
                    name: "never_registered".into(),
                    args: serde_json::json!({}),
                    provider_data: serde_json::Value::Null,
                },
                &ToolContext::default(),
            )
            .await;
        assert!(forbidden.is_error);
        assert!(unknown.is_error);
        // Same "unknown tool" verdict for both; the only difference is the
        // name echo, which the model itself supplied in the call.
        assert_eq!(forbidden.content, "unknown tool: boom");
        assert_eq!(unknown.content, "unknown tool: never_registered");
    }
}
