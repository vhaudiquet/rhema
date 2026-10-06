//! The approval bridge between the turn task and the UI thread.
//!
//! DEADLOCK CONSTRAINT: [`Approval::approve`] is synchronous and runs on
//! the turn task while the UI thread keeps rendering. The bridge therefore
//! sends an [`UiEvent::ApprovalRequested`] over the (unbounded) event
//! channel and blocks on a std mpsc receiver until the UI answers. This is
//! deadlock-free under exactly two invariants:
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

use std::sync::{Mutex, MutexGuard, PoisonError};

use rhema_coding_agent::Approval;
use rhema_core::ToolCall;
use tokio::sync::mpsc::UnboundedSender;

use crate::events::UiEvent;

/// The UI-side half of the bridge: sends the user's answer for the request
/// the turn task is currently blocked on. Dropping it declines every
/// present and future request (fail-safe).
pub struct ApprovalAnswerer {
    tx: std::sync::mpsc::Sender<bool>,
}

impl ApprovalAnswerer {
    /// Answers the pending approval request; a send to a closed channel is
    /// silently ignored (the turn is already declining).
    pub fn answer(&self, approved: bool) {
        let _ = self.tx.send(approved);
    }
}

/// The turn-side [`Approval`]: forwards the request to the UI, blocks for
/// the answer, declines on any failure.
pub struct TuiApproval {
    events: UnboundedSender<UiEvent>,
    // Only the turn task touches the receiver; the mutex exists because
    // `approve` takes `&self`.
    answers: Mutex<std::sync::mpsc::Receiver<bool>>,
}

impl TuiApproval {
    /// Pairs a turn-side approval with its UI-side answerer. `events` is
    /// the UI event channel the request shows up on.
    pub fn pair(events: UnboundedSender<UiEvent>) -> (Self, ApprovalAnswerer) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Self {
                events,
                answers: Mutex::new(rx),
            },
            ApprovalAnswerer { tx },
        )
    }

    fn answers(&self) -> MutexGuard<'_, std::sync::mpsc::Receiver<bool>> {
        self.answers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Approval for TuiApproval {
    fn approve(&self, call: &ToolCall) -> bool {
        // A closed event channel means the UI is gone: decline without
        // blocking, so the turn unwinds instead of parking forever.
        if self
            .events
            .send(UiEvent::ApprovalRequested { call: call.clone() })
            .is_err()
        {
            return false;
        }
        // Blocks the turn task's worker until the UI thread answers; see
        // the module docs for why that cannot deadlock. `Err` is a closed
        // channel (UI quit): decline.
        self.answers().recv() == Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::ToolCall;
    use serde_json::json;
    use std::sync::Arc;

    fn call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            args: json!({ "path": "notes.txt" }),
        }
    }

    #[test]
    fn approve_returns_the_ui_answer() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (approval, answers) = TuiApproval::pair(tx);

        // Blocks like a real turn task would.
        let handle = std::thread::spawn(move || approval.approve(&call()));

        // The UI side sees the request first, then answers.
        match rx.blocking_recv() {
            Some(UiEvent::ApprovalRequested { call: received }) => {
                assert_eq!(received.name, "write_file");
            }
            _ => panic!("the parked approval must send its request"),
        }
        answers.answer(true);
        assert!(handle.join().unwrap(), "the answered approval passes");
    }

    #[test]
    fn a_decline_answer_declines() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (approval, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || approval.approve(&call()));
        assert!(rx.blocking_recv().is_some());
        answers.answer(false);
        assert!(!handle.join().unwrap());
    }

    #[test]
    fn one_hook_serves_sequential_requests() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (approval, answers) = TuiApproval::pair(tx);
        let approval = Arc::new(approval);

        // Approvals are sequential on the turn task: one request, one
        // answer, then the next.
        let a = approval.clone();
        let first = std::thread::spawn(move || a.approve(&call()));
        assert!(rx.blocking_recv().is_some());
        answers.answer(true);
        assert!(first.join().unwrap());

        let b = approval.clone();
        let second = std::thread::spawn(move || b.approve(&call()));
        assert!(rx.blocking_recv().is_some());
        answers.answer(false);
        assert!(!second.join().unwrap());
    }

    #[test]
    fn dropping_the_answerer_declines_a_parked_approval() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (approval, answers) = TuiApproval::pair(tx);
        let handle = std::thread::spawn(move || approval.approve(&call()));
        // The UI goes away while the approval is parked: the channel
        // closes, `recv` errors, the call declines.
        drop(answers);
        assert!(!handle.join().unwrap(), "EOF must decline (fail-safe)");
    }

    #[test]
    fn a_closed_ui_channel_declines_without_parking() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let (approval, _answers) = TuiApproval::pair(tx);
        assert!(!approval.approve(&call()));
    }
}
