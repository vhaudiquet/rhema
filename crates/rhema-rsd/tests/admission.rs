//! Concurrency caps (spec §6): the global cap, per-source caps, the FIFO
//! admission queue, and the admission-TTL return to the debounce path.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{ADMISSION, DEBOUNCE, config, event, register_github, setup_with};
use rhema_rsd::{DispatchError, DispatcherConfig, KeyFn, KeyState, Outcome, ResourceKey};

/// Note on ordering: `"gitea:..."` sorts before `"github:..."`, and #142
/// before #143, so due-key FIFO order is deterministic in every test here.
fn github_key(number: u32) -> ResourceKey {
    ResourceKey::from(format!("github:acme/widgets#{number}"))
}

fn gitea_key(number: u32) -> ResourceKey {
    ResourceKey::from(format!("gitea:acme/other#{number}"))
}

fn gitea_key_fn() -> Arc<KeyFn> {
    Arc::new(|event| ResourceKey::from(event.payload["key"].as_str().unwrap_or("gitea:unknown")))
}

/// A github event naming `key`.
fn gitea_event(key: &ResourceKey, id: &str) -> common::Event {
    let mut event = event(key.as_str(), id);
    event.source = "gitea".into();
    event
}

#[test]
fn acquiring_past_the_global_cap_is_refused() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: 1,
        ..config()
    });
    register_github(&dispatcher);
    let k1 = github_key(142);
    let k2 = github_key(143);
    dispatcher.ingest(event(k1.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    dispatcher.acquire(k1, "handler-1").unwrap();

    match dispatcher.acquire(k2, "handler-2") {
        Err(DispatchError::AtCapacity { scope }) => assert_eq!(scope, None),
        other => panic!("expected AtCapacity, got {other:?}"),
    }
}

#[test]
fn blocked_keys_wait_in_fifo_admission_order() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: 1,
        ..config()
    });
    register_github(&dispatcher);
    let k1 = github_key(142);
    let k2 = github_key(143);
    dispatcher.ingest(event(k1.as_str(), "e1")).unwrap();
    dispatcher.ingest(event(k2.as_str(), "e2")).unwrap();

    clock.advance(DEBOUNCE);
    // One slot, two due keys: the first is offered, the second is queued.
    assert_eq!(dispatcher.admit().unwrap(), vec![k1.clone()]);
    let status = dispatcher.status(&k2).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert!(status.admitted_at.is_some());

    // The slot is taken; k2 keeps waiting, inbox intact.
    let (lease1, _session) = dispatcher.acquire(k1.clone(), "handler-1").unwrap();
    assert!(dispatcher.admit().unwrap().is_empty());
    assert_eq!(dispatcher.status(&k2).unwrap().inbox_depth, 1);

    // When the slot frees, the queued key is the one offered.
    dispatcher.poll_inbox(&lease1).unwrap();
    dispatcher.release(&lease1, Outcome::Handled).unwrap();
    assert_eq!(dispatcher.admit().unwrap(), vec![k2]);
}

#[test]
fn an_admission_ttl_expiry_sends_the_key_back_through_debounce() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: 1,
        ..config()
    });
    register_github(&dispatcher);
    let k1 = github_key(142);
    let k2 = github_key(143);
    dispatcher.ingest(event(k1.as_str(), "e1")).unwrap();
    dispatcher.ingest(event(k2.as_str(), "e2")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease1, _session) = dispatcher.acquire(k1.clone(), "handler-1").unwrap();

    // k2's queue entry expires while k1 is still leased: it leaves the
    // queue and re-enters the debounce path, as if its event just arrived.
    clock.advance(ADMISSION);
    assert!(dispatcher.admit().unwrap().is_empty());
    let status = dispatcher.status(&k2).unwrap();
    assert_eq!(status.admitted_at, None, "expired entry left the queue");
    assert_eq!(
        status.debounce_deadline,
        Some(1_000 + DEBOUNCE + ADMISSION + DEBOUNCE),
    );

    // Not dropped: once the slot frees and the fresh window elapses, the
    // key dispatches with its events still pending.
    dispatcher.poll_inbox(&lease1).unwrap();
    dispatcher.release(&lease1, Outcome::Handled).unwrap();
    clock.advance(DEBOUNCE);
    assert_eq!(dispatcher.admit().unwrap(), vec![k2]);
}

#[test]
fn per_source_caps_hold_while_other_sources_proceed() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_per_source: BTreeMap::from([("github".into(), 1)]),
        ..config()
    });
    register_github(&dispatcher);
    dispatcher.register_source("gitea", gitea_key_fn());
    let k_github_1 = github_key(142);
    let k_github_2 = github_key(143);
    let k_gitea = gitea_key(7);
    dispatcher.ingest(event(k_github_1.as_str(), "e1")).unwrap();
    dispatcher.ingest(event(k_github_2.as_str(), "e2")).unwrap();
    dispatcher.ingest(gitea_event(&k_gitea, "e3")).unwrap();

    clock.advance(DEBOUNCE);
    // github is capped at one; gitea has its own budget. FIFO order puts
    // the gitea key first, so it and the first github key are offered and
    // the second github key queues despite free global slots.
    let admitted = dispatcher.admit().unwrap();
    assert_eq!(admitted, vec![k_gitea.clone(), k_github_1.clone()]);
    assert!(
        dispatcher
            .status(&k_github_2)
            .unwrap()
            .admitted_at
            .is_some()
    );

    // The cap is authoritative at acquire too.
    dispatcher.acquire(k_github_1, "h1").unwrap();
    match dispatcher.acquire(k_github_2, "h2") {
        Err(DispatchError::AtCapacity { scope }) => {
            assert_eq!(scope.as_deref(), Some("github"))
        }
        other => panic!("expected per-source AtCapacity, got {other:?}"),
    }
    dispatcher.acquire(k_gitea, "h3").unwrap();
}

#[test]
fn a_late_event_while_the_cap_is_taken_queues_for_the_next_slot() {
    let (dispatcher, clock) = setup_with(DispatcherConfig {
        max_concurrent_leases: 1,
        ..config()
    });
    register_github(&dispatcher);
    let k1 = github_key(142);
    let k2 = github_key(143);
    dispatcher.ingest(event(k1.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    dispatcher.acquire(k1.clone(), "handler-1").unwrap();

    // k2 arrives while the only slot is taken; after its debounce it waits
    // in the queue rather than racing the lease.
    dispatcher.ingest(event(k2.as_str(), "e2")).unwrap();
    clock.advance(DEBOUNCE);
    assert!(dispatcher.admit().unwrap().is_empty());
    let status = dispatcher.status(&k2).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert!(status.admitted_at.is_some());
}
