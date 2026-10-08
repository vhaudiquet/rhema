//! Shared helpers for the rhema-autonomous-agent integration suites: a
//! memory-store dispatcher with a hand-driven clock, one registered source
//! whose key function reads the payload's `key` field, and the events to
//! feed it.

// Each test binary includes this module and uses a different subset.
#![allow(dead_code)]

use std::sync::Arc;

use rhema_rsd::{Dispatcher, DispatcherConfig, Event, FixedClock, KeyFn, MemoryStore, ResourceKey};

/// The suite's source name.
pub const SOURCE: &str = "github";
/// Debounce window used across suites (milliseconds).
pub const DEBOUNCE: u64 = 100;
/// Lease TTL used across suites (milliseconds).
pub const TTL: u64 = 10_000;
/// Admission TTL used across suites (milliseconds).
pub const ADMISSION: u64 = 500;

/// Suite-standard config: spec-default caps with short, test-sized windows.
pub fn config() -> DispatcherConfig {
    DispatcherConfig {
        debounce_window_millis: DEBOUNCE,
        lease_ttl_millis: TTL,
        admission_ttl_millis: ADMISSION,
        ..DispatcherConfig::default()
    }
}

/// A dispatcher over a fresh memory store, plus its clock (parked at
/// t = 1_000).
pub fn setup() -> (Arc<Dispatcher>, Arc<FixedClock>) {
    setup_with(config())
}

/// Like [`setup`], with a custom config.
pub fn setup_with(config: DispatcherConfig) -> (Arc<Dispatcher>, Arc<FixedClock>) {
    let clock = Arc::new(FixedClock::new(1_000));
    let dispatcher = Arc::new(Dispatcher::new(
        config,
        clock.clone(),
        Arc::new(MemoryStore::new()),
    ));
    (dispatcher, clock)
}

/// Registers the suite's source: the resource key is the payload's `key`
/// field.
pub fn register(dispatcher: &Dispatcher) {
    dispatcher.register_source(SOURCE, key_fn());
}

/// The suite's key function, exposed separately so a [`Source`] contract
/// test can compare a source's own report against it.
pub fn key_fn() -> Arc<KeyFn> {
    Arc::new(|event| ResourceKey::from(event.payload["key"].as_str().unwrap_or("unknown")))
}

/// A source event naming `key`, with `id` as its dedup id.
pub fn event(key: &str, id: &str) -> Event {
    Event {
        id: id.into(),
        source: SOURCE.into(),
        payload: serde_json::json!({ "key": key }),
        received_at: 0,
    }
}

/// The suite's stock resource key.
pub fn the_key() -> ResourceKey {
    ResourceKey::from("github:acme/widgets#142")
}
