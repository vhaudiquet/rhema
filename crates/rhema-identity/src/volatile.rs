//! The in-process [`IdentityStore`]: a mutex-protected map, no durability
//! beyond process lifetime. Backs tests and single-process embedders; the
//! SQLite store replays the same contract.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::record::{Account, AccountDraft, IdCounter, Secret, check_id};
use crate::store::{IdentityError, IdentityStore};

/// One stored entry: the redacted view plus its secret, kept together so
/// an account and its credential are never out of step.
#[derive(Clone, Debug)]
struct Entry {
    account: Account,
    secret: Secret,
}

/// An in-process [`IdentityStore`]. Shareable; every operation is
/// serialized by an internal mutex, so all of its mutations are atomic.
#[derive(Debug, Default)]
pub struct VolatileIdentityStore {
    entries: Mutex<BTreeMap<String, Entry>>,
    ids: IdCounter,
}

impl VolatileIdentityStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl IdentityStore for VolatileIdentityStore {
    fn put(&self, draft: AccountDraft) -> Result<Account, IdentityError> {
        let norm = draft.validate()?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let now = SystemTime::now();
        let now_ms = now_millis();
        match draft.id {
            Some(id) => {
                let entry = entries
                    .get_mut(&id)
                    .ok_or_else(|| IdentityError::UnknownId { id: id.clone() })?;
                entry.account = Account {
                    id: id.clone(),
                    label: norm.label,
                    service: norm.service,
                    url: norm.url,
                    username: norm.username,
                    secret_kind: norm.secret.kind,
                    created_at: entry.account.created_at,
                    updated_at: now_ms,
                };
                entry.secret = norm.secret;
                Ok(entry.account.clone())
            }
            None => {
                let id = loop {
                    let candidate = self.ids.fresh(now);
                    if !entries.contains_key(&candidate) {
                        break candidate;
                    }
                };
                let entry = Entry {
                    account: Account {
                        id: id.clone(),
                        label: norm.label,
                        service: norm.service,
                        url: norm.url,
                        username: norm.username,
                        secret_kind: norm.secret.kind,
                        created_at: now_ms,
                        updated_at: now_ms,
                    },
                    secret: norm.secret,
                };
                entries.insert(id, entry.clone());
                Ok(entry.account)
            }
        }
    }

    fn get(&self, id: &str) -> Result<Option<Account>, IdentityError> {
        check_id(id)?;
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries.get(id).map(|entry| entry.account.clone()))
    }

    fn list(&self) -> Result<Vec<Account>, IdentityError> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries
            .values()
            .map(|entry| entry.account.clone())
            .collect())
    }

    fn forget(&self, id: &str) -> Result<bool, IdentityError> {
        check_id(id)?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries.remove(id).is_some())
    }

    fn set_secret(&self, id: &str, secret: Secret) -> Result<Account, IdentityError> {
        check_id(id)?;
        let value = secret.value.trim();
        if value.is_empty() {
            return Err(IdentityError::EmptySecret);
        }
        if value.len() > crate::SECRET_LIMIT {
            return Err(IdentityError::SecretTooLong {
                len: value.len(),
                limit: crate::SECRET_LIMIT,
            });
        }
        let secret = Secret {
            kind: secret.kind,
            value: value.to_owned(),
        };
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| IdentityError::UnknownId { id: id.to_owned() })?;
        entry.secret = secret;
        entry.account.secret_kind = entry.secret.kind;
        entry.account.updated_at = now_millis();
        Ok(entry.account.clone())
    }

    fn secret(&self, id: &str) -> Result<Option<Secret>, IdentityError> {
        check_id(id)?;
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(entries.get(id).map(|entry| entry.secret.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::SecretKind;
    use crate::store_contract;

    fn draft(label: &str) -> AccountDraft {
        AccountDraft::new(label, "github", Secret::new(SecretKind::Token, "tok"))
    }

    #[test]
    fn the_contract_holds() {
        store_contract::run_all(&VolatileIdentityStore::new());
    }

    #[test]
    fn a_secret_round_trips_and_dies_with_the_account() {
        let store = VolatileIdentityStore::new();
        let account = store.put(draft("acme").with_username("octocat")).unwrap();
        let secret = store.secret(&account.id).unwrap().expect("secret stored");
        assert_eq!(secret.kind, SecretKind::Token);
        assert_eq!(secret.value, "tok");

        assert!(store.forget(&account.id).unwrap());
        assert_eq!(store.secret(&account.id).unwrap(), None);
    }

    #[test]
    fn a_debug_dump_of_the_store_carries_no_secret() {
        let store = VolatileIdentityStore::new();
        store
            .put(AccountDraft::new(
                "acme",
                "github",
                Secret::new(SecretKind::Password, "hunter2"),
            ))
            .unwrap();
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }
}
