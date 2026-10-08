//! The in-process [`MemoryStore`]: a mutex-protected map, no durability
//! beyond process lifetime. Backs tests and single-process embedders; the
//! markdown-file store replays the same contract.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::record::{IdCounter, MemoryDraft, MemoryRecord, check_id, normalize_tags};
use crate::search::{MemoryQuery, rank};
use crate::store::{MemoryError, MemoryStore};

/// An in-process [`MemoryStore`]. Shareable; every operation is
/// serialized by an internal mutex, so all of its mutations are atomic.
#[derive(Debug, Default)]
pub struct VolatileStore {
    records: Mutex<BTreeMap<String, MemoryRecord>>,
    ids: IdCounter,
}

impl VolatileStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_millis(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl MemoryStore for VolatileStore {
    fn put(&self, draft: MemoryDraft) -> Result<MemoryRecord, MemoryError> {
        draft.validate()?;
        let tags = normalize_tags(&draft.tags);
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let now = SystemTime::now();
        let now_ms = now_millis(now);
        match draft.id {
            Some(id) => {
                let existing = records
                    .get_mut(&id)
                    .ok_or_else(|| MemoryError::UnknownId { id: id.clone() })?;
                existing.scope = draft.scope;
                existing.tags = tags;
                existing.body = draft.body;
                existing.updated_at = now_ms;
                Ok(existing.clone())
            }
            None => {
                let id = loop {
                    let candidate = self.ids.fresh(now);
                    if !records.contains_key(&candidate) {
                        break candidate;
                    }
                };
                let record = MemoryRecord {
                    id,
                    scope: draft.scope,
                    tags,
                    body: draft.body,
                    created_at: now_ms,
                    updated_at: now_ms,
                };
                records.insert(record.id.clone(), record.clone());
                Ok(record)
            }
        }
    }

    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        check_id(id)?;
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.get(id).cloned())
    }

    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(rank(records.values().cloned().collect(), &query))
    }

    fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        check_id(id)?;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.remove(id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Scope;
    use crate::store_contract;

    #[test]
    fn the_contract_holds() {
        store_contract::run_all(&VolatileStore::new());
    }

    #[test]
    fn ranking_orders_terms_recency_and_tags() {
        store_contract::rank_orders_terms_recency_and_tags(&VolatileStore::new());
    }

    #[test]
    fn a_search_reflects_the_latest_update() {
        let store = VolatileStore::new();
        let record = store
            .put(MemoryDraft::new(Scope::Global, "wasm target is v1"))
            .unwrap();
        store
            .put(MemoryDraft::new(Scope::Global, "wasm target is v2").with_id(record.id.clone()))
            .unwrap();
        let found = store.search(MemoryQuery::new("wasm")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "wasm target is v2");
        assert_eq!(found[0].created_at, record.created_at);
        assert!(found[0].updated_at >= found[0].created_at);
    }
}
