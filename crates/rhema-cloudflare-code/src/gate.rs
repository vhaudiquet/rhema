//! The approval gate for a runtime with one thread.
//!
//! [`Approval::approve`] is synchronous by contract, and the existing
//! frontends block inside it — the ACP adapter parks a worker thread
//! until the editor answers. workerd has no second thread to park: a
//! native block there would stall the isolate and deadlock the very
//! socket the answer arrives on. So the cloud gate asks *before* the
//! tool resolution starts, at an await point the turn task can suspend
//! on: [`ClientGate`] is a [`ToolResolver`] that shadows `bash`,
//! obtains a decision (static policy first, the connected client
//! second, deny last), and only then delegates to the inner
//! [`ShellTools`] with a per-call hook. The spec, the caps, and the
//! rejection message stay [`ShellTools`]' — nothing forks.
#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use rhema_agent_tools::{AllowAll, ShellTools};
use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};

use crate::assembly::ApprovalPolicy;

/// Asks the connected client about one call. `None` means nobody is
/// connected or the answer never came — read as a denial. The
/// implementation owns its own timeout.
#[async_trait]
pub trait ApprovalChannel: Send + Sync {
    /// The client's answer, `None` for no client or no answer.
    async fn ask(&self, call: &ToolCall) -> Option<bool>;
}

/// The gate every gated call passes: static policy, then the client
/// channel, then the denial. Install it as the extension resolver over
/// the agent's [`ShellTools`] — it shadows `bash` with the same spec,
/// so prompt and tools array still cannot drift.
pub struct ClientGate {
    inner: Arc<ShellTools>,
    policy: ApprovalPolicy,
    channel: Option<Arc<dyn ApprovalChannel>>,
}

impl ClientGate {
    /// A gate over `inner` — the agent's own [`ShellTools`] — answering
    /// from `policy`, consulting `channel` for the calls the policy
    /// hands to the client (`AskClient`).
    pub fn new(
        inner: Arc<ShellTools>,
        policy: ApprovalPolicy,
        channel: Option<Arc<dyn ApprovalChannel>>,
    ) -> Self {
        Self {
            inner,
            policy,
            channel,
        }
    }

    /// The decision for one call. `AskClient` without a channel denies:
    /// nobody is listening.
    async fn decision(&self, call: &ToolCall) -> bool {
        match &self.policy {
            ApprovalPolicy::DenyAll => false,
            ApprovalPolicy::AllowList(names) => names.contains(&call.name),
            ApprovalPolicy::AllowAll => true,
            ApprovalPolicy::AskClient => match &self.channel {
                Some(channel) => channel.ask(call).await.unwrap_or(false),
                None => false,
            },
        }
    }

    /// Delegates to the inner shell with the approval pre-installed: a
    /// call only reaches here after `decision` answered yes, so the
    /// per-call hook is an allow and the shell's own gate passes.
    async fn resolve_decided(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        self.inner.install_approval(Arc::new(AllowAll));
        let result = self.inner.resolve_streaming(call, ctx, progress).await;
        // Back to the fail-safe: with the per-call hook cleared, the
        // turn-level hook — deny on this frontend — governs anything
        // that bypasses this gate.
        self.inner.clear_approval();
        result
    }
}

#[async_trait]
impl ToolResolver for ClientGate {
    fn specs(&self) -> Vec<ToolSpec> {
        // The inner shell's own specs: one source of truth.
        self.inner.specs()
    }

    fn replay_safety(&self, name: &str) -> rhema_core::ReplaySafety {
        self.inner.replay_safety(name)
    }

    async fn resolve(&self, call: ToolCall, ctx: &ToolContext) -> ToolResult {
        // A fn item, not a closure: its lifetime-generic signature is
        // what the higher-ranked progress bound needs.
        fn silent(_: &str) {}
        self.resolve_decided(call, ctx, &mut silent).await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> ToolResult {
        if ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        if !self.decision(&call).await {
            // Same text the shell's own gate rejects with.
            return ToolResult::failure(&call.id, "rejected by user approval");
        }
        self.resolve_decided(call, ctx, progress).await
    }
}
