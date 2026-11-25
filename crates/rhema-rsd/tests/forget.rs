//! Deleting a key's record: the gated delete, its two answers, the refusal a
//! live lease earns, and the lapsed lease that must not block one.

mod common;

use std::sync::Arc;

use common::{TTL, config, event, register, the_key};
use rhema_rsd::{
    Conflict, DispatchError, Dispatcher, FixedClock, IngestResult, KeyState, Session, Store,
    VolatileStore,
};

/// A dispatcher over a store the test keeps, so a delete can be checked
/// against the store and not only through `status`. The clock is parked at
/// t = 1_000, as in the other suites.
fn harness() -> (Arc<Dispatcher>, Arc<FixedClock>, Arc<VolatileStore>) {
    let store = Arc::new(VolatileStore::new());
    let clock = Arc::new(FixedClock::new(1_000));
    let dispatcher = Arc::new(Dispatcher::new(config(), clock.clone(), store.clone()));
    register(&dispatcher);
    (dispatcher, clock, store)
}

#[test]
fn forgetting_a_key_with_no_record_deletes_nothing() {
    let (dispatcher, _clock, store) = harness();
    let key = the_key();
    assert!(
        !dispatcher.forget(&key).unwrap(),
        "a key with no record deletes nothing"
    );
    assert!(store.keys().unwrap().is_empty());
}

#[test]
fn an_idle_record_forgets_and_the_key_starts_empty() {
    let (dispatcher, _clock, store) = harness();
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    assert_eq!(store.keys().unwrap(), vec![key.clone()]);

    assert!(
        dispatcher.forget(&key).unwrap(),
        "an existing record is removed"
    );
    assert!(
        store.keys().unwrap().is_empty(),
        "the store no longer lists it"
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    // A second delete on the same key reports what the store now holds.
    assert!(!dispatcher.forget(&key).unwrap());

    // A key re-created under the same name starts empty: the deleted inbox
    // does not remember e1, so the replay is not a duplicate.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e1")).unwrap(),
        IngestResult::DebouncePending
    );
}

#[test]
fn a_live_lease_refuses_the_delete_and_the_record_survives() {
    let (dispatcher, _clock, store) = harness();
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    let (lease, _session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();

    match dispatcher.forget(&key) {
        Err(DispatchError::Conflict(Conflict::LeaseHeld { lease_id, holder })) => {
            assert_eq!(lease_id, lease.id);
            assert_eq!(holder, "handler-1");
        }
        other => panic!("expected LeaseHeld, got {other:?}"),
    }
    // Untouched: still leased, still stored, and the turn's own write still
    // lands, the reason the refusal exists.
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    assert_eq!(store.keys().unwrap(), vec![key.clone()]);
    dispatcher
        .checkpoint(&lease, Session::fresh(key.clone(), 0))
        .unwrap();
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
}

#[test]
fn a_lapsed_lease_does_not_block_the_delete() {
    let (dispatcher, clock, store) = harness();
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    let (lease, _session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();

    // Nobody swept the key: the record still says leased when the delete
    // arrives, which is what a crashed daemon leaves behind.
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    clock.advance(TTL);
    assert!(
        dispatcher.forget(&key).unwrap(),
        "a dead lease blocks nothing"
    );
    assert!(store.keys().unwrap().is_empty());

    // The zombie handler resurrects nothing: its checkpoint answers an error
    // and errors persist nothing.
    assert!(
        dispatcher
            .checkpoint(&lease, Session::fresh(key.clone(), 0))
            .is_err()
    );
    assert!(store.keys().unwrap().is_empty(), "the delete holds");
}
