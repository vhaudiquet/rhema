//! Steering: a chat message that cuts a running turn short and runs
//! instead of it — the same interrupt rhema-code's UI owns, wired
//! through the HTTP surface.
//!
//! The dispatch model is queue-shaped: a message that arrives while a
//! key's turn is live lands in the key's inbox and waits for the next
//! dispatch. For a resident session that can be a long wait — one turn
//! runs every tool round up to `max_turn_steps` — and a user typing
//! "stop, do X instead" into a chat that keeps going is the exact
//! frustration steering exists to end. So the messages route, after its
//! ingest, signals this registry; a turn whose dispatch holds a
//! listener for the key takes the signal, drops its `run_turn` future
//! at the current await (an in-flight shell command dies with it —
//! `kill_on_drop`), repairs the dangling calls, and loops: the inbox
//! drain at the top of the next round picks the steered message up as
//! the next turn's input, same lease, same session, same stream.
//!
//! The contract between the route and the loop has one ordering rule:
//! **signal only after ingest**. A signal corresponds to a message
//! already in the inbox, so the loop may clear stale signals freely —
//! each one is a duplicate of something a drain will find anyway. That
//! is what keeps the capacity-one channel honest: two quick messages
//! turn into one interrupt and one batched input, never a lost message.
//!
//! Signalling a key with no live dispatch is a harmless no-op (the
//! message simply waits for admission, as it always did), which makes
//! the route's call unconditional: it cannot know whether a turn runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// The per-key interrupt registry. The daemon builds one; the
/// interactive handler listens on it, the messages route signals
/// through it.
#[derive(Default)]
pub struct SteerRegistry {
    /// One sender per key with a dispatch in flight. The lease
    /// guarantees at most one dispatch per key, so entries never
    /// collide; the guard's `Drop` removes its own.
    live: Mutex<HashMap<String, mpsc::Sender<()>>>,
}

impl SteerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interrupts the live dispatch on `key`, if there is one. `true`
    /// when a listener took the signal; `false` when no turn runs (or
    /// its channel is already full — one pending interrupt is enough,
    /// the message itself is in the inbox either way).
    pub fn signal(&self, key: &str) -> bool {
        let listener = self
            .live
            .lock()
            .expect("steer registry lock")
            .get(key)
            .cloned();
        match listener {
            Some(sender) => sender.try_send(()).is_ok(),
            None => false,
        }
    }

    /// Arms a listener for `key`. The guard is the dispatch's side: hold
    /// it across the whole dispatch chain, drop it to unregister.
    pub fn listen(self: &Arc<Self>, key: &str) -> SteerGuard {
        let (sender, receiver) = mpsc::channel(1);
        self.live
            .lock()
            .expect("steer registry lock")
            .insert(key.to_owned(), sender.clone());
        SteerGuard {
            registry: Arc::clone(self),
            key: key.to_owned(),
            sender,
            receiver: Some(receiver),
        }
    }
}

/// One dispatch's listener: the receiver the select waits on, plus
/// enough identity to unregister only its own entry.
///
/// The receiver sits in an `Option` because the wait helper takes it by
/// value (`recv` consumes); a borrow of the guard hands it back after
/// every await, so the guard stays the one object the dispatch loop
/// carries from round to round.
pub struct SteerGuard {
    registry: Arc<SteerRegistry>,
    key: String,
    /// The guard's own sender, kept only so `Drop` can tell its entry
    /// from a later dispatch's.
    sender: mpsc::Sender<()>,
    receiver: Option<mpsc::Receiver<()>>,
}

impl SteerGuard {
    /// Whether a signal is pending — the completion-race check: a
    /// message that arrived while the turn was finishing steers the
    /// next round instead of slipping into the closing window.
    pub fn take(&mut self) -> bool {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.try_recv().is_ok(),
            None => false,
        }
    }

    /// Resolves once a signal arrives. A closed channel — impossible
    /// while this guard lives, since its own sender sits in the
    /// registry — parks forever rather than resolving empty, so the
    /// select's steer arm goes silent instead of spinning.
    pub async fn wait(&mut self) {
        loop {
            let received = match self.receiver.as_mut() {
                Some(receiver) => receiver.recv().await,
                None => std::future::pending::<Option<()>>().await,
            };
            if received.is_some() {
                return;
            }
        }
    }

    /// Drains every pending signal. The dispatch loop runs this at the
    /// top of each round: signals only arrive after their message's
    /// ingest, so anything pending here is already waiting in the inbox,
    /// and a stale signal must not cut the round it precedes.
    pub fn clear(&mut self) {
        while self.take() {}
    }
}

impl Drop for SteerGuard {
    fn drop(&mut self) {
        // Only remove the entry this guard inserted: a same-key entry
        // from a later dispatch must survive (it cannot coexist with a
        // live one — the lease forbids it — but a defensive check keeps
        // the registry honest regardless of caller discipline).
        let mut live = self.registry.live.lock().expect("steer registry lock");
        if live
            .get(&self.key)
            .is_some_and(|entry| entry.same_channel(&self.sender))
        {
            live.remove(&self.key);
        }
        // Dropping the receiver first is not required (the sender in the
        // map is a clone), but taking it out of the guard closes the
        // channel for any late `wait`.
        self.receiver = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_with_no_listener_is_a_no_op() {
        let registry = Arc::new(SteerRegistry::new());
        assert!(!registry.signal("interactive:web"));
    }

    #[tokio::test]
    async fn a_listener_takes_the_signal_and_unregisters_on_drop() {
        let registry = Arc::new(SteerRegistry::new());
        let mut guard = registry.listen("interactive:web");

        assert!(registry.signal("interactive:web"));
        guard.wait().await;
        // The channel is drained now; another signal fits.
        assert!(registry.signal("interactive:web"));
        assert!(guard.take());

        // A second signal with no consumer left fills the capacity-one
        // channel and reports as not taken.
        assert!(registry.signal("interactive:web"));
        assert!(!registry.signal("interactive:web"));

        drop(guard);
        assert!(!registry.signal("interactive:web"), "the entry is gone");
    }

    #[tokio::test]
    async fn clear_empties_pending_signals() {
        let registry = Arc::new(SteerRegistry::new());
        let mut guard = registry.listen("interactive:web");
        assert!(registry.signal("interactive:web"));
        guard.clear();
        assert!(!guard.take(), "nothing is pending after a clear");
    }

    #[test]
    fn keys_steer_independently() {
        let registry = Arc::new(SteerRegistry::new());
        let _guard = registry.listen("interactive:web");
        assert!(registry.signal("interactive:web"));
        assert!(!registry.signal("interactive:desk"));
    }
}
