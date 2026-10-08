//! Concurrency smoke test: many tokio tasks racing one shared dispatcher,
//! acquiring distinct keys while the global cap holds.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{DEBOUNCE, event, register_github, setup_with};
use rhema_core::{Message, Role};
use rhema_rsd::{DispatcherConfig, KeyState, Outcome, ResourceKey};

#[tokio::test]
async fn concurrent_tasks_acquire_distinct_keys_under_the_global_cap() {
    const TASKS: usize = 6;
    const CAP: usize = 2;

    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: CAP as u32,
        debounce_window_millis: 0, // due immediately: the race is on acquire
        ..common::config()
    });
    register_github(&dispatcher);

    // All events land up front, so every task's key is due before any of
    // them acquires.
    let keys: Vec<ResourceKey> = (0..TASKS)
        .map(|i| ResourceKey::from(format!("github:acme/widgets#{i}")))
        .collect();
    for (i, key) in keys.iter().enumerate() {
        dispatcher
            .ingest(event(key.as_str(), &format!("e{i}")))
            .unwrap();
    }
    clock.advance(DEBOUNCE);

    let live = Arc::new(AtomicUsize::new(0));
    let max_live = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for key in keys.clone() {
        let dispatcher = dispatcher.clone();
        let live = live.clone();
        let max_live = max_live.clone();
        handles.push(tokio::spawn(async move {
            // Each task admits until its own key is offered, then does one
            // checkpointed turn of work and releases. Admit is advisory, so
            // a LeaseHeld/AtCapacity race just means retry on a later pass.
            for _ in 0..2_000 {
                let due = dispatcher.admit().unwrap();
                if due.contains(&key) {
                    if let Ok((lease, mut session)) = dispatcher.acquire(key.clone(), "task") {
                        let now_live = live.fetch_add(1, Ordering::SeqCst) + 1;
                        max_live.fetch_max(now_live, Ordering::SeqCst);
                        // The triggering event is still pending: drain it.
                        assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
                        session
                            .core
                            .history
                            .push(Message::text(Role::User, "one turn of work"));
                        dispatcher.checkpoint(&lease, session).unwrap();
                        dispatcher.renew(&lease).unwrap();
                        live.fetch_sub(1, Ordering::SeqCst);
                        dispatcher.release(&lease, Outcome::Handled).unwrap();
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
            panic!("key {key} was never admitted and acquired");
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    // Every key was served exactly once, the cap never overflowed, and the
    // board is back to idle.
    assert!(max_live.load(Ordering::SeqCst) <= CAP);
    for key in &keys {
        let status = dispatcher.status(key).unwrap();
        assert_eq!(status.state, KeyState::Idle);
        assert_eq!(status.inbox_depth, 0);
        assert_eq!(status.revision, 1);
        assert_eq!(status.last_outcome, Some(Outcome::Handled));
    }
}
