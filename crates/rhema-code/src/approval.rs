//! The approval bridge between the turn task and the UI thread.
//!
//! Production runs every consumer with `AllowAll` by design, so no
//! turn-side [`Approval`] hook is wired today; the modal, the answer
//! channel, and this module's UI-side half wait for the real flow.
//!
//! DEADLOCK CONSTRAINT for whoever wires it: [`Approval::approve`] is
//! synchronous and runs on the turn task while the UI thread keeps
//! rendering. The bridge must send an `UiEvent::ApprovalRequested`
//! over the (unbounded) event channel and block on a std mpsc receiver
//! until the UI answers. That is deadlock-free under exactly two
//! invariants:
//!
//! 1. The UI thread NEVER blocks on the turn task. It drains its event
//!    channel with `try_recv` (or an async recv with timeout) every loop
//!    iteration and answers through a non-blocking `Sender::send` — and it
//!    keeps draining while a modal is up, which it does anyway because the
//!    modal lives in the same loop.
//! 2. Only the turn task uses the answer receiver. One turn = one approval
//!    hook = one consumer; sequential approvals line up FIFO with the
//!    answers, so no id matching is needed.
//!
//! Fail-safety: dropping the UI's [`ApprovalAnswerer`] (quit, UI thread
//! gone) closes the channel; `recv()` then errors and every call declines.
//! A closed event channel (UI gone before the request was sent) declines
//! too.
//!
//! Blocking a tokio worker thread on the std receiver is safe here because
//! the answer always comes from the UI thread — a separate OS thread that
//! does not depend on the runtime making progress.

/// The UI-side half of the bridge: sends the user's answer for the request
/// the turn task is currently blocked on. Dropping it declines every
/// present and future request (fail-safe).
pub struct ApprovalAnswerer {
    tx: std::sync::mpsc::Sender<bool>,
}

impl ApprovalAnswerer {
    /// Builds the answerer over one end of a std channel; the other
    /// end is the receiver a turn-side hook parks on. Nothing wires a
    /// hook today; tests build the pair to exercise the modal.
    #[cfg(test)]
    pub(crate) fn sender(tx: std::sync::mpsc::Sender<bool>) -> Self {
        Self { tx }
    }

    /// Answers the pending approval request; a send to a closed channel is
    /// silently ignored (the turn is already declining).
    pub fn answer(&self, approved: bool) {
        let _ = self.tx.send(approved);
    }
}
