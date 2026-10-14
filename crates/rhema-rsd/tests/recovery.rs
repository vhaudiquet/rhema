//! Crash recovery (spec §7): a restart sweeps abandoned leases back to
//! idle, or to re-acquire when their inbox holds undelivered events, and
//! the session survives into the next acquisition.

mod common;

use std::sync::Arc;

use common::{DEBOUNCE, TTL, config, event, register_github, the_key};
use rhema_core::{Message, Role};
use rhema_rsd::{Dispatcher, FixedClock, KeyState, Outcome, VolatileStore};

/// Builds a store as if a process died mid-work: a leased key with a
/// checkpointed session, optionally an undelivered inbox item, and a clock
/// sitting past the lease TTL.
fn crashed(with_unpolled_event: bool) -> (Arc<VolatileStore>, Arc<FixedClock>) {
    let clock = Arc::new(FixedClock::new(1_000));
    let store = Arc::new(VolatileStore::new());
    let dispatcher = Arc::new(Dispatcher::new(config(), clock.clone(), store.clone()));
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
    session
        .core
        .history
        .push(Message::text(Role::User, "half the work is done"));
    session.artifact_ref = Some("worktrees/issue-142".into());
    dispatcher.checkpoint(&lease, session).unwrap();
    if with_unpolled_event {
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap();
    }
    clock.advance(TTL + 1);
    (store, clock)
}

#[test]
fn recover_resets_an_expired_empty_lease_to_idle() {
    let (store, clock) = crashed(false);
    let restarted = Dispatcher::new(config(), clock.clone(), store);
    let key = the_key();

    restarted.recover().unwrap();
    let status = restarted.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.lease, None);
    assert_eq!(status.debounce_deadline, None);
}

#[test]
fn recover_sends_an_expired_lease_with_queued_events_to_reacquire() {
    let (store, clock) = crashed(true);
    let restarted = Dispatcher::new(config(), clock.clone(), store);
    let key = the_key();

    restarted.recover().unwrap();
    let status = restarted.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Reacquire);
    assert_eq!(status.inbox_depth, 1);
    assert!(status.debounce_deadline.is_some());

    // After the debounce window the key dispatches, and the new handler
    // resumes the checkpointed session.
    clock.advance(DEBOUNCE);
    assert_eq!(restarted.admit().unwrap(), vec![key.clone()]);
    let (lease, session) = restarted.acquire(key.clone(), "handler-2").unwrap();
    assert_eq!(lease.revision, 2);
    let first = session.core.history.first().expect("history survived");
    assert_eq!(first.role, Role::User);
    assert_eq!(session.artifact_ref.as_deref(), Some("worktrees/issue-142"));
    // The unpolled comment survived the crash for the new handler.
    assert_eq!(restarted.poll_inbox(&lease).unwrap().len(), 1);
    restarted.release(&lease, Outcome::Handled).unwrap();
}

#[test]
fn recover_leaves_a_live_lease_alone() {
    let (dispatcher, clock) = common::setup();
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    dispatcher.acquire(key.clone(), "handler-1").unwrap();

    // Well within the TTL: recovery must not touch the live lease.
    clock.advance(TTL / 2);
    dispatcher.recover().unwrap();
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    assert_eq!(dispatcher.list_active().unwrap().len(), 1);
}

#[test]
fn list_active_reports_only_leased_keys_without_sweeping() {
    let (dispatcher, clock) = common::setup();
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    dispatcher.acquire(key.clone(), "handler-1").unwrap();

    clock.advance(TTL + 1);
    // Pure read: the lapsed lease still shows until a sweep runs.
    assert_eq!(dispatcher.list_active().unwrap().len(), 1);
    dispatcher.recover().unwrap();
    assert!(dispatcher.list_active().unwrap().is_empty());
}

/// Builds the same crash as [`crashed`], but with the lease still well
/// inside its TTL — the shape a daemon restart actually leaves behind:
/// the process died, its lease has hours left on the clock.
fn crashed_fresh_lease(with_unpolled_event: bool) -> (Arc<VolatileStore>, Arc<FixedClock>) {
    let clock = Arc::new(FixedClock::new(1_000));
    let store = Arc::new(VolatileStore::new());
    let dispatcher = Arc::new(Dispatcher::new(config(), clock.clone(), store.clone()));
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
    session
        .core
        .history
        .push(Message::text(Role::User, "half the work is done"));
    dispatcher.checkpoint(&lease, session).unwrap();
    if with_unpolled_event {
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap();
    }
    // A sliver of the TTL, not its end: the lease is fresh.
    clock.advance(TTL / 2);
    (store, clock)
}

/// The strict recovery: a restart over a single-node store releases a
/// dead process's lease immediately, not at the dead lease's TTL — an
/// hour on the resident dispatcher would stall every key the cut turn
/// held.
#[test]
fn recover_all_releases_a_fresh_lease_of_a_dead_process() {
    let (store, clock) = crashed_fresh_lease(false);
    let restarted = Dispatcher::new(config(), clock.clone(), store);
    let key = the_key();

    // The TTL recovery deliberately leaves the fresh lease: only
    // recover_all asserts the holder is gone.
    restarted.recover().unwrap();
    assert_eq!(restarted.status(&key).unwrap().state, KeyState::Leased);

    restarted.recover_all().unwrap();
    let status = restarted.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.lease, None);
}

#[test]
fn recover_all_sends_queued_events_to_reacquire_within_the_ttl() {
    let (store, clock) = crashed_fresh_lease(true);
    let restarted = Dispatcher::new(config(), clock.clone(), store);
    let key = the_key();

    restarted.recover_all().unwrap();
    let status = restarted.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Reacquire);
    assert_eq!(status.inbox_depth, 1);
    assert!(status.debounce_deadline.is_some());

    // The new process dispatches after the debounce and resumes the
    // checkpointed session with the queued event.
    clock.advance(DEBOUNCE);
    assert_eq!(restarted.admit().unwrap(), vec![key.clone()]);
    let (lease, session) = restarted.acquire(key.clone(), "handler-2").unwrap();
    let first = session.core.history.first().expect("history survived");
    assert_eq!(first.role, Role::User);
    assert_eq!(restarted.poll_inbox(&lease).unwrap().len(), 1);
    restarted.release(&lease, Outcome::Handled).unwrap();
}
