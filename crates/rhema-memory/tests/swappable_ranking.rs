//! The swappability tripwire: the order-agnostic contract suite must
//! pass against a store whose ranking is deliberately *wrong*.
//!
//! `MemoryStore::search` promises ranked-best-first and nothing about the
//! method, so no caller may depend on rank internals. If anyone adds an
//! order assertion to `run_all`, this store fails it — the tripwire that
//! keeps the suite replayable by the semantic ranker the framework spec
//! defers to later.

use std::sync::Arc;

use rhema_memory::{
    MemoryDraft, MemoryQuery, MemoryRecord, MemoryStore, VolatileStore, store_contract,
};

/// Delegates everything to a [`VolatileStore`] but reverses every
/// keyword search. The empty query — recent — is contract, not rank
/// internals, so it passes through untouched.
struct Scrambled(Arc<VolatileStore>);

impl MemoryStore for Scrambled {
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, rhema_memory::MemoryError> {
        self.0.put(draft)
    }

    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, rhema_memory::MemoryError> {
        self.0.get(id)
    }

    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, rhema_memory::MemoryError> {
        let keyword = !query.text.trim().is_empty();
        let mut found = self.0.search(query)?;
        if keyword {
            found.reverse();
        }
        Ok(found)
    }

    fn forget(&self, id: &str) -> Result<bool, rhema_memory::MemoryError> {
        self.0.forget(id)
    }
}

#[test]
fn the_order_agnostic_suite_passes_on_a_scrambled_ranker() {
    let store = Scrambled(Arc::new(VolatileStore::new()));
    store_contract::run_all(&store);
}
