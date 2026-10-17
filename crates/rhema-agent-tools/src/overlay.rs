//! The two-resolver combiner: built-in tools plus an extension set.
//!
//! [`Overlay`] merges a primary resolver (the built-ins) with an extra
//! one (an extension's tools): the model sees one spec list, and a call
//! goes to whichever side offers the name. An extra shadows a primary of
//! the same name — the extension wins, both in the list the model sees
//! and in dispatch — so a frontend can replace a built-in without
//! touching the primary.

use std::sync::Arc;

use async_trait::async_trait;

use rhema_core::{ReplaySafety, ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

/// A primary resolver plus an extension resolver, presented as one.
pub struct Overlay {
    /// The built-in side; listed first so the prompt's tool order stays
    /// stable.
    primary: Arc<dyn ToolResolver>,
    /// The extension side; shadows the primary by name.
    extra: Arc<dyn ToolResolver>,
}

impl Overlay {
    /// Combines `primary` with `extra`; the extra wins name collisions.
    pub fn new(primary: Arc<dyn ToolResolver>, extra: Arc<dyn ToolResolver>) -> Self {
        Self { primary, extra }
    }

    /// Whether `resolver` offers a tool named `name`.
    fn offers(resolver: &dyn ToolResolver, name: &str) -> bool {
        resolver.specs().iter().any(|spec| spec.name == name)
    }

    /// The side a call for `name` belongs to.
    fn side(&self, name: &str) -> &Arc<dyn ToolResolver> {
        if Self::offers(self.extra.as_ref(), name) {
            &self.extra
        } else {
            &self.primary
        }
    }
}

#[async_trait]
impl ToolResolver for Overlay {
    fn specs(&self) -> Vec<ToolSpec> {
        let extra = self.extra.specs();
        let shadowed: Vec<&str> = extra.iter().map(|spec| spec.name.as_str()).collect();
        let mut specs: Vec<ToolSpec> = self
            .primary
            .specs()
            .into_iter()
            .filter(|spec| !shadowed.contains(&spec.name.as_str()))
            .collect();
        specs.extend(extra);
        specs
    }

    fn replay_safety(&self, name: &str) -> ReplaySafety {
        let side = self.side(name);
        if Arc::ptr_eq(side, &self.extra) {
            self.extra.replay_safety(name)
        } else {
            self.primary.replay_safety(name)
        }
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        self.side(&call.name).resolve(call, ctx).await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        self.side(&call.name)
            .resolve_streaming(call, ctx, progress)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use rhema_core::ToolResult;
    use serde_json::json;

    /// A configurable test resolver: offers `names`, records resolved
    /// calls, answers `safety` for everything it offers.
    struct Fake {
        names: Vec<&'static str>,
        safety: ReplaySafety,
        resolved: Mutex<Vec<String>>,
    }

    impl Fake {
        /// An `Arc` over a fake offering `names`.
        fn resolver(names: Vec<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                names,
                safety: ReplaySafety::Unsafe,
                resolved: Mutex::new(Vec::new()),
            })
        }

        /// The names resolved through this side so far.
        fn taken(&self) -> Vec<String> {
            self.resolved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl ToolResolver for Fake {
        fn specs(&self) -> Vec<ToolSpec> {
            self.names
                .iter()
                .map(|name| ToolSpec::new(*name, "a fake tool", json!({"type": "object"})))
                .collect()
        }

        fn replay_safety(&self, name: &str) -> ReplaySafety {
            if self.names.contains(&name) {
                self.safety
            } else {
                ReplaySafety::Unsafe
            }
        }

        async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
            self.resolved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(call.name);
            ToolResult::success(call.id, "done")
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            name: name.to_owned(),
            ..ToolCall::default()
        }
    }

    #[tokio::test]
    async fn the_extra_shadows_a_primary_of_the_same_name() {
        let primary = Fake::resolver(vec!["bash"]);
        let extra = Fake::resolver(vec!["bash"]);
        let overlay = Overlay::new(primary.clone(), extra.clone());
        let specs = overlay.specs();
        assert_eq!(specs.len(), 1, "one spec, not two: {specs:?}");
        let result = overlay.resolve(call("bash"), &Default::default()).await;
        assert!(!result.is_error);
        assert!(primary.taken().is_empty(), "the extra resolved it");
        assert_eq!(extra.taken(), vec!["bash"]);
    }

    #[tokio::test]
    async fn dispatch_reaches_whichever_side_offers_the_name() {
        let primary = Fake::resolver(vec!["bash"]);
        let extra = Fake::resolver(vec!["subagent"]);
        let overlay = Overlay::new(primary.clone(), extra.clone());
        let ctx = ToolContext::default();
        overlay.resolve(call("bash"), &ctx).await;
        overlay.resolve(call("subagent"), &ctx).await;
        assert_eq!(primary.taken(), vec!["bash"]);
        assert_eq!(extra.taken(), vec!["subagent"]);
    }

    #[tokio::test]
    async fn streaming_dispatch_follows_the_same_side() {
        let primary = Fake::resolver(vec!["bash"]);
        let extra = Fake::resolver(vec!["subagent"]);
        let overlay = Overlay::new(primary.clone(), extra.clone());
        let ctx = ToolContext::default();
        let result = overlay
            .resolve_streaming(call("subagent"), &ctx, &mut |_| {})
            .await;
        assert!(!result.is_error);
        assert!(primary.taken().is_empty());
        assert_eq!(extra.taken(), vec!["subagent"]);
    }

    #[test]
    fn specs_list_the_primary_first_then_the_extras() {
        let overlay = Overlay::new(
            Fake::resolver(vec!["bash", "other"]),
            Fake::resolver(vec!["subagent"]),
        );
        let specs = overlay.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["bash", "other", "subagent"]);
    }

    #[test]
    fn replay_safety_follows_the_dispatch_side() {
        let extra = Arc::new(Fake {
            names: vec!["query"],
            safety: ReplaySafety::Safe,
            resolved: Mutex::new(Vec::new()),
        });
        let overlay = Overlay::new(Fake::resolver(vec!["bash"]), extra);
        assert_eq!(overlay.replay_safety("query"), ReplaySafety::Safe);
        assert_eq!(overlay.replay_safety("bash"), ReplaySafety::Unsafe);
        // An unknown name is unclassified on both sides: the default.
        assert_eq!(overlay.replay_safety("mystery"), ReplaySafety::Unsafe);
    }
}
