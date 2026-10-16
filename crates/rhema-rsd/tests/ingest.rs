//! Ingest behavior: debounce coalescing of an idle burst, dedup of replayed
//! event ids, live delivery onto a leased key, and unknown sources.

mod common;

use common::{DEBOUNCE, Event, event, register_github, setup, the_key};
use rhema_rsd::{DispatchError, IngestResult, KeyState};

#[test]
fn a_burst_at_an_idle_key_coalesces_into_one_dispatch() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();

    for id in ["e1", "e2", "e3"] {
        assert_eq!(
            dispatcher.ingest(event(key.as_str(), id)).unwrap(),
            IngestResult::DebouncePending
        );
    }
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 3);

    // Before the window elapses nothing is due.
    clock.advance(DEBOUNCE - 1);
    assert!(dispatcher.admit().unwrap().is_empty());

    // One dispatch, carrying the whole burst: a single acquire, and the
    // handler's first poll folds all three events at once.
    clock.advance(1);
    assert_eq!(dispatcher.admit().unwrap(), vec![key.clone()]);
    let (lease, _session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    let items = dispatcher.poll_inbox(&lease).unwrap();
    let ids: Vec<&str> = items.iter().map(|item| item.event.id.as_str()).collect();
    assert_eq!(ids, ["e1", "e2", "e3"]);
}

#[test]
fn a_replayed_event_id_is_deduplicated() {
    let (dispatcher, _clock) = setup();
    register_github(&dispatcher);
    let key = the_key();

    assert_eq!(
        dispatcher
            .ingest(event(key.as_str(), "delivery-7"))
            .unwrap(),
        IngestResult::DebouncePending
    );
    // Webhook redelivery of the same id while the first copy is still
    // pending: dropped.
    assert_eq!(
        dispatcher
            .ingest(event(key.as_str(), "delivery-7"))
            .unwrap(),
        IngestResult::Duplicate
    );
    assert_eq!(dispatcher.status(&key).unwrap().inbox_depth, 1);
}

#[test]
fn an_event_while_leased_lands_live_without_a_state_change() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, _session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();

    // The handler drains its triggering event, so the inbox is empty...
    let items = dispatcher.poll_inbox(&lease).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].event.id, "e1");

    // ...and a new event then lands live: appended with no state change
    // and no debounce.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap(),
        IngestResult::DeliveredLive
    );
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Leased);
    assert_eq!(status.inbox_depth, 1);
    assert!(status.debounce_deadline.is_none());
    let items = dispatcher.poll_inbox(&lease).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].event.id, "e2");
}

#[test]
fn an_unknown_source_is_rejected() {
    let (dispatcher, _clock) = setup();
    register_github(&dispatcher);

    let event = Event {
        id: "e1".into(),
        source: "telegram-inbound".into(),
        payload: serde_json::json!({}),
        received_at: 0,
    };
    match dispatcher.ingest(event) {
        Err(DispatchError::UnknownSource(source)) => assert_eq!(source, "telegram-inbound"),
        other => panic!("expected UnknownSource, got {other:?}"),
    }
}
