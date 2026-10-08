//! The bundled memory store replays the reusable Store-contract suite —
//! the same suite a durable backend (SQLite) replays to prove itself.

#![cfg(feature = "store-contract")]

use rhema_rsd::{VolatileStore, store_contract};

#[test]
fn load_missing_returns_none_for_the_memory_store() {
    store_contract::load_missing_returns_none(&VolatileStore::new());
}

#[test]
fn save_then_load_round_trips_for_the_memory_store() {
    store_contract::save_then_load_round_trips(&VolatileStore::new());
}

#[test]
fn fresh_save_requires_absence_for_the_memory_store() {
    store_contract::fresh_save_requires_absence(&VolatileStore::new());
}

#[test]
fn cas_rejects_stale_revision_for_the_memory_store() {
    store_contract::cas_rejects_stale_revision(&VolatileStore::new());
}

#[test]
fn keys_lists_all_recorded_keys_for_the_memory_store() {
    store_contract::keys_lists_all_recorded_keys(&VolatileStore::new());
}

#[test]
fn the_memory_store_satisfies_the_whole_contract() {
    store_contract::run_all(&VolatileStore::new());
}
