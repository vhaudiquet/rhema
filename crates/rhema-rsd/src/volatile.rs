//! The in-process [`Store`]: a mutex-protected map, no durability beyond
//! process lifetime. Backs tests and single-process embedders; a durable
//! backend (SQLite) replays the same contract via
//! [`crate::store_contract`].

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::StoreError;
use crate::key::ResourceKey;
use crate::state::Revision;
use crate::store::{KeyRecord, Store};

/// An in-memory [`Store`]. Shareable; every operation is serialized by an
/// internal mutex, so all of its mutations are atomic.
#[derive(Debug, Default)]
pub struct VolatileStore {
    records: Mutex<BTreeMap<ResourceKey, KeyRecord>>,
}

impl VolatileStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for VolatileStore {
    fn load(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, StoreError> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.get(key).cloned())
    }

    fn save(
        &self,
        record: KeyRecord,
        expected_revision: Option<Revision>,
    ) -> Result<(), StoreError> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let actual = records.get(&record.key).map(|stored| stored.revision);
        if actual != expected_revision {
            return Err(StoreError::RevisionMismatch {
                expected: expected_revision.unwrap_or_default(),
                actual: actual.unwrap_or_default(),
            });
        }
        records.insert(record.key.clone(), record);
        Ok(())
    }

    fn keys(&self) -> Result<Vec<ResourceKey>, StoreError> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(records.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::KeyState;

    #[test]
    fn save_and_load_round_trip_one_record() {
        let store = VolatileStore::new();
        let key = ResourceKey::from("k");
        let mut record = KeyRecord::fresh(key.clone(), 7);
        record.state = KeyState::Leased;
        store.save(record.clone(), None).unwrap();
        assert_eq!(store.load(&key).unwrap(), Some(record));
        assert_eq!(store.keys().unwrap(), vec![key]);
    }

    #[test]
    fn a_missing_key_loads_as_none() {
        let store = VolatileStore::new();
        assert_eq!(store.load(&ResourceKey::from("absent")).unwrap(), None);
    }
}
