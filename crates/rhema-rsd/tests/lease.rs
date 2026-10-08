//! Lease lifecycle: renewals, TTL expiry and sweeping, acquisition races,
//! and revision-guarded checkpoints and releases.

mod common;

use std::sync::Arc;

use common::{DEBOUNCE, TTL, config, event, register_github, setup, the_key};
use rhema_core::Message;
use rhema_rsd::{
    Conflict, DispatchError, Dispatcher, Expired, FixedClock, IngestResult, KeyState, Lease,
    Outcome, ResourceKey, Session,
};

/// A leased key: the assignment event dispatched, acquired, and drained at
/// t = 1_100, so the board starts clean.
fn leased() -> (Arc<Dispatcher>, Arc<FixedClock>, ResourceKey, Lease) {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, _session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
    (dispatcher, clock, key, lease)
}

#[test]
fn renew_extends_the_lease_window() {
    let (dispatcher, clock, _key, lease) = leased();

    // Halfway through the TTL a renewal is fine and pushes the deadline out.
    clock.advance(TTL / 2);
    let renewed = dispatcher.renew(&lease).unwrap();
    assert_eq!(renewed.id, lease.id);
    assert_eq!(renewed.revision, lease.revision);
    assert_eq!(renewed.expires_at(), clock.now_millis() + TTL);

    // A moment past the original deadline is still live thanks to the
    // renewal.
    clock.advance(TTL - TTL / 2);
    assert!(dispatcher.renew(&renewed).is_ok());
}

#[test]
fn renewal_after_expiry_fails() {
    let (dispatcher, clock, _key, lease) = leased();
    clock.advance(TTL);
    match dispatcher.renew(&lease) {
        Err(DispatchError::Expired(Expired { revision })) => assert_eq!(revision, 1),
        other => panic!("expected Expired, got {other:?}"),
    }
}

#[test]
fn an_expired_lease_is_swept_and_the_slot_frees() {
    let (dispatcher, clock, key, lease) = leased();

    clock.advance(TTL);
    // admit runs the lazy sweep: empty inbox, so the key goes idle.
    dispatcher.admit().unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.lease, None);

    // The lapsed lease can no longer poll or renew...
    assert!(dispatcher.poll_inbox(&lease).unwrap().is_empty());
    assert!(dispatcher.renew(&lease).is_err());

    // ...and an event now debounces instead of delivering live: the lease
    // is gone.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap(),
        IngestResult::DebouncePending
    );
}

#[test]
fn acquiring_a_leased_key_reports_the_holder() {
    let (dispatcher, _clock, key, _lease) = leased();
    match dispatcher.acquire(key, "handler-2") {
        Err(DispatchError::Conflict(Conflict::LeaseHeld { holder, .. })) => {
            assert_eq!(holder, "handler-1")
        }
        other => panic!("expected LeaseHeld, got {other:?}"),
    }
}

#[test]
fn a_key_past_its_ttl_can_be_acquired_directly() {
    let (dispatcher, clock, key, _lease) = leased();
    clock.advance(TTL + 1);
    // The sweep inside acquire frees the lapsed lease without waiting for
    // an admit pass.
    let (lease2, _session) = dispatcher.acquire(key, "handler-2").unwrap();
    assert_eq!(lease2.revision, 2);
}

#[test]
fn a_checkpoint_on_a_stolen_revision_is_rejected() {
    let (dispatcher, clock, key, lease1) = leased();

    // The lease lapses, the sweep frees the key, a second handler takes it
    // at revision 2 and checkpoints newer state.
    clock.advance(TTL + 1);
    dispatcher.admit().unwrap();
    let (lease2, mut session2) = dispatcher.acquire(key.clone(), "handler-2").unwrap();
    assert_eq!(lease2.revision, 2);
    session2
        .core
        .history
        .push(Message::text(rhema_core::Role::User, "from handler-2"));
    dispatcher.checkpoint(&lease2, session2).unwrap();

    // The stale handler must abort rather than overwrite newer state.
    let stale = Session::fresh(key, 0);
    match dispatcher.checkpoint(&lease1, stale) {
        Err(DispatchError::Conflict(Conflict::RevisionMismatch { expected, actual })) => {
            assert_eq!((expected, actual), (1, 2));
        }
        other => panic!("expected RevisionMismatch, got {other:?}"),
    }
}

#[test]
fn a_checkpoint_after_own_expiry_reports_expired() {
    let (dispatcher, clock, key, lease) = leased();
    clock.advance(TTL + 1);
    // Nobody re-acquired: the revision still matches, but the sweep ended
    // our lease, so the failure is expiry, not a revision conflict.
    match dispatcher.checkpoint(&lease, Session::fresh(key, 0)) {
        Err(DispatchError::Expired(_)) => {}
        other => panic!("expected Expired, got {other:?}"),
    }
}

#[test]
fn an_expired_lease_may_still_release_cleanly() {
    let (dispatcher, clock, key, lease) = leased();
    clock.advance(TTL + 1);
    // No sweep has run yet: the lease record still stands, so a zombie
    // handler cleaning up is still the lease's identity — allowed.
    dispatcher
        .release(&lease, Outcome::Failed("overslept".into()))
        .unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(
        status.last_outcome,
        Some(Outcome::Failed("overslept".into()))
    );
}

#[test]
fn a_release_from_a_superseded_lease_is_a_revision_conflict() {
    let (dispatcher, clock, key, lease1) = leased();
    clock.advance(TTL + 1);
    dispatcher.admit().unwrap();
    let (lease2, _session) = dispatcher.acquire(key.clone(), "handler-2").unwrap();
    assert_eq!(lease2.revision, 2);
    match dispatcher.release(&lease1, Outcome::Handled) {
        Err(DispatchError::Conflict(Conflict::RevisionMismatch { expected, actual })) => {
            assert_eq!((expected, actual), (1, 2))
        }
        other => panic!("expected RevisionMismatch, got {other:?}"),
    }
    // The live lease is unaffected.
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
}

#[test]
fn two_dispatchers_share_one_store_but_not_a_private_one() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    dispatcher.acquire(key.clone(), "handler-1").unwrap();

    // A dispatcher over a different store knows nothing of the key.
    let elsewhere = Dispatcher::new(config(), clock, Arc::new(rhema_rsd::MemoryStore::new()));
    let status = elsewhere.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.revision, 0);
}
