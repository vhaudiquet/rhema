//! The approval seam: a per-turn hook that gates the side-effecting tools.
//!
//! The hook travels as an `Arc` because the resolver that consults it is
//! owned by the engine's `Agent` and must stay `'static` — a plain `&dyn`
//! borrow cannot be installed into it. It is installed before a turn and
//! cleared after; tool resolution is synchronous within a turn, so the
//! per-turn swap is race-free.

use rhema_core::ToolCall;

/// Decides whether a gated tool call (`bash`) may run.
/// Called once per gated call, before execution.
pub trait Approval: Send + Sync {
    /// Return `true` to let the tool run.
    fn approve(&self, call: &ToolCall) -> bool;
}

/// Approves everything; used by non-interactive drivers and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Approval for AllowAll {
    fn approve(&self, _call: &ToolCall) -> bool {
        true
    }
}

/// Rejects every gated call: the headless default, where nobody is
/// present to approve. Pair with an explicit opt-in to run tools
/// unattended.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl Approval for DenyAll {
    fn approve(&self, _call: &ToolCall) -> bool {
        false
    }
}
