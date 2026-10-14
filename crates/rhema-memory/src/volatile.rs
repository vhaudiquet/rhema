//! The in-process [`MemoryStore`]: a mutex-protected map, no durability
//! beyond process lifetime. Backs tests and single-process embedders; the
//! markdown-file store replays the same contract.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::record::{
    IdCounter, MemoryDraft, MemoryRecord, Status, check_id, compose, normalize_tags,
};
use crate::review::ReviewItem;
use crate::search::{MemoryQuery, select};
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
        let draft_id = draft.id.clone();
        let (id, existing) = match draft_id {
            Some(id) => {
                let existing = records
                    .remove(&id)
                    .ok_or_else(|| MemoryError::UnknownId { id: id.clone() })?;
                (id, Some(existing))
            }
            None => {
                let id = loop {
                    let candidate = self.ids.fresh(now);
                    if !records.contains_key(&candidate) {
                        break candidate;
                    }
                };
                (id, None)
            }
        };
        let created_at = existing.as_ref().map_or(now_ms, |e| e.created_at);
        let record = compose(&draft, tags, existing, id, created_at, now_ms);
        records.insert(record.id.clone(), record.clone());
        Ok(record)
    }

    fn get(&self, id: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        check_id(id)?;
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.get(id).cloned())
    }

    fn search(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(select(
            records.values().cloned().collect(),
            &query,
            now_millis(SystemTime::now()),
        ))
    }

    fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        check_id(id)?;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.remove(id).is_some())
    }

    fn supersede(&self, id: &str, by: &str) -> Result<MemoryRecord, MemoryError> {
        check_id(id)?;
        check_id(by)?;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let now_ms = now_millis(SystemTime::now());
        let record = records
            .get_mut(id)
            .ok_or_else(|| MemoryError::UnknownId { id: id.to_owned() })?;
        record.status = Status::Superseded {
            by: by.to_owned(),
            at: now_ms,
        };
        record.updated_at = now_ms;
        Ok(record.clone())
    }

    fn retract(&self, id: &str, why: &str) -> Result<MemoryRecord, MemoryError> {
        check_id(id)?;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let now_ms = now_millis(SystemTime::now());
        let record = records
            .get_mut(id)
            .ok_or_else(|| MemoryError::UnknownId { id: id.to_owned() })?;
        record.status = Status::Retracted {
            why: why.to_owned(),
            at: now_ms,
        };
        record.updated_at = now_ms;
        Ok(record.clone())
    }

    fn verify(&self, id: &str) -> Result<MemoryRecord, MemoryError> {
        check_id(id)?;
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let now_ms = now_millis(SystemTime::now());
        let record = records
            .get_mut(id)
            .ok_or_else(|| MemoryError::UnknownId { id: id.to_owned() })?;
        record.verified_at = Some(now_ms);
        Ok(record.clone())
    }

    fn due_for_review(&self, now: u64) -> Result<Vec<ReviewItem>, MemoryError> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(crate::review::due_for_review(
            records.values().cloned().collect(),
            now,
        ))
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
        store_contract::run_rank_orders(&VolatileStore::new());
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
