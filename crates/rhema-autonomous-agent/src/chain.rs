//! Resolver composition: several resolvers behind one seam.
//!
//! The resident agent's toolset is naturally layered — the coding tools
//! (shell and friends) plus agent-side capabilities like watch
//! management — but `rhema_core` ships exactly one resolver seam and no
//! composition, on purpose. The chain lives here because composing
//! toolsets is an agent-tier concern.

use std::sync::Arc;

use async_trait::async_trait;
use rhema_core::{ReplaySafety, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

/// Several resolvers behind one seam: specs concatenate in order, and a
/// call is served by the first resolver offering that tool's name. Tool
/// names should be unique across the chain; on a duplicate, the earlier
/// resolver wins and the later one is unreachable for it.
pub struct ResolverChain {
    resolvers: Vec<Arc<dyn ToolResolver>>,
}

impl ResolverChain {
    /// A chain serving tools from `resolvers`, in order.
    pub fn new(resolvers: Vec<Arc<dyn ToolResolver>>) -> Self {
        Self { resolvers }
    }
}

#[async_trait]
impl ToolResolver for ResolverChain {
    fn specs(&self) -> Vec<ToolSpec> {
        self.resolvers
            .iter()
            .flat_map(|resolver| resolver.specs())
            .collect()
    }

    fn replay_safety(&self, name: &str) -> ReplaySafety {
        self.resolvers
            .iter()
            .find(|resolver| resolver.specs().iter().any(|spec| spec.name == name))
            .map(|resolver| resolver.replay_safety(name))
            .unwrap_or(ReplaySafety::Unsafe)
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        for resolver in &self.resolvers {
            if resolver.specs().iter().any(|spec| spec.name == call.name) {
                return resolver.resolve(call, ctx).await;
            }
        }
        ToolResult::failure(&call.id, format!("unknown tool: {}", call.name))
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        for resolver in &self.resolvers {
            if resolver.specs().iter().any(|spec| spec.name == call.name) {
                return resolver.resolve_streaming(call, ctx, progress).await;
            }
        }
        ToolResult::failure(&call.id, format!("unknown tool: {}", call.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ToolResult as CoreToolResult;
    use serde_json::Value;

    struct Fixed {
        name: &'static str,
        answer: &'static str,
    }

    #[async_trait]
    impl ToolResolver for Fixed {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec::new(self.name, "fixed", serde_json::json!({}))]
        }
        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> CoreToolResult {
            CoreToolResult::success(&call.id, self.answer)
        }
    }

    #[tokio::test]
    async fn a_call_is_served_by_the_first_resolver_offering_the_name() {
        let chain = ResolverChain::new(vec![
            Arc::new(Fixed {
                name: "bash",
                answer: "from bash",
            }),
            Arc::new(Fixed {
                name: "watch_repo",
                answer: "from watch",
            }),
        ]);
        let ctx = ToolContext::default();
        let served = chain
            .resolve(
                ToolCall {
                    id: "c1".into(),
                    name: "watch_repo".into(),
                    args: serde_json::json!({}),
                    provider_data: Value::Null,
                },
                &ctx,
            )
            .await;
        assert_eq!(served.content, "from watch");
    }

    #[tokio::test]
    async fn an_unknown_tool_fails_in_band() {
        let chain = ResolverChain::new(vec![Arc::new(Fixed {
            name: "bash",
            answer: "x",
        })]);
        let ctx = ToolContext::default();
        let served = chain
            .resolve(
                ToolCall {
                    id: "c1".into(),
                    name: "nope".into(),
                    args: serde_json::json!({}),
                    provider_data: Value::Null,
                },
                &ctx,
            )
            .await;
        assert!(served.is_error);
    }

    /// A resolver that classifies one of its tools replay-safe, so the
    /// chain's forwarding has something to find.
    struct Classified {
        name: &'static str,
        safety: ReplaySafety,
    }

    #[async_trait]
    impl ToolResolver for Classified {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec::new(
                self.name,
                "classified",
                serde_json::json!({}),
            )]
        }
        fn replay_safety(&self, _name: &str) -> ReplaySafety {
            self.safety
        }
        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> CoreToolResult {
            CoreToolResult::success(&call.id, "x")
        }
    }

    #[test]
    fn replay_safety_routes_to_the_tool_owning_resolver() {
        let chain = ResolverChain::new(vec![
            Arc::new(Fixed {
                name: "bash",
                answer: "x",
            }),
            Arc::new(Classified {
                name: "zoom",
                safety: ReplaySafety::Safe,
            }),
        ]);
        assert_eq!(chain.replay_safety("zoom"), ReplaySafety::Safe);
        assert_eq!(
            chain.replay_safety("bash"),
            ReplaySafety::Unsafe,
            "an unclassified tool stays side-effecting"
        );
        assert_eq!(chain.replay_safety("nope"), ReplaySafety::Unsafe);
    }

    #[tokio::test]
    async fn specs_concatenate_in_order() {
        let chain = ResolverChain::new(vec![
            Arc::new(Fixed {
                name: "bash",
                answer: "x",
            }),
            Arc::new(Fixed {
                name: "watch_repo",
                answer: "x",
            }),
        ]);
        let names: Vec<String> = chain.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["bash".to_owned(), "watch_repo".to_owned()]);
    }
}
