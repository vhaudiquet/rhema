//! The replayable [`Store`] behavioral suite.
//!
//! Every function here is a plain assertion-based test body taking any
//! `&dyn Store`; the bundled [`crate::MemoryStore`] runs them from this
//! crate's own tests, and a second store implementation (the planned SQLite
//! backend) replays the exact same suite by dev-dependending on `rhema-rsd`
//! with `features = ["store-contract"]` and calling them from its own
//! `#[test]`s:
//!
//! ```text
//! #[test]
//! fn sqlite_store_satisfies_the_contract() {
//!     let store = SqliteStore::open(":memory:");
//!     rhema_rsd::store_contract::run_all(&store);
//! }
//! ```
//!
//! Each function uses its own key namespace so they compose on one store;
//! `run_all` expects a fresh store and runs everything in order.

use serde_json::json;

use crate::StoreError;
use crate::event::{Event, InboxItem};
use crate::key::ResourceKey;
use crate::lease::LeaseId;
use crate::session::Session;
use crate::state::{KeyState, Outcome};
use crate::store::{KeyRecord, LeaseRecord, Store};

/// [`Store::load`] on a never-written key returns `None`.
pub fn load_missing_returns_none(store: &dyn Store) {
    let key = ResourceKey::from("contract/missing");
    assert_eq!(store.load(&key).unwrap(), None);
}

/// A fully populated record survives `save` + `load` field-for-field, inbox
/// order included.
pub fn save_then_load_round_trips(store: &dyn Store) {
    let key = ResourceKey::from("contract/round-trip");
    let record = KeyRecord {
        key: key.clone(),
        state: KeyState::Leased,
        revision: 3,
        lease: Some(LeaseRecord {
            id: LeaseId::from("lease-9"),
            holder: "handler-1".into(),
            acquired_at: 10,
            expires_at: 25,
        }),
        inbox: vec![
            InboxItem {
                event: Event {
                    id: "e1".into(),
                    source: "github".into(),
                    payload: json!({ "first": true }),
                    received_at: 5,
                },
                delivered_at: 6,
            },
            InboxItem {
                event: Event {
                    id: "e2".into(),
                    source: "github".into(),
                    payload: json!({ "first": false }),
                    received_at: 7,
                },
                delivered_at: 8,
            },
        ],
        source: "github".into(),
        session: Some(Session {
            key: key.clone(),
            core: rhema_core::Session {
                history: vec![rhema_core::Message::text(
                    rhema_core::Role::User,
                    "resume here",
                )],
                driver_state: json!({ "opaque": 1 }),
            },
            plan: Some(json!(["step one", "step two"])),
            artifact_ref: Some("worktrees/issue-142".into()),
            created_at: 1,
            last_active: 9,
        }),
        debounce_deadline: None,
        admitted_at: Some(11),
        last_outcome: Some(Outcome::Failed("boom".into())),
        updated_at: 12,
    };
    store.save(record.clone(), None).unwrap();
    assert_eq!(store.load(&key).unwrap(), Some(record));
}

/// A conditional create succeeds only against a missing key; both a second
/// create and a conditional save on a missing key fail the CAS.
pub fn fresh_save_requires_absence(store: &dyn Store) {
    let key = ResourceKey::from("contract/fresh");
    let record = KeyRecord::fresh(key.clone(), 1);
    store.save(record.clone(), None).unwrap();

    let duplicate = store.save(record, None).unwrap_err();
    assert!(
        matches!(
            duplicate,
            StoreError::RevisionMismatch {
                expected: 0,
                actual: 0
            }
        ),
        "second create must fail the CAS, got {duplicate:?}"
    );

    let other = ResourceKey::from("contract/fresh-other");
    let missing = store.save(KeyRecord::fresh(other, 1), Some(0)).unwrap_err();
    assert!(
        matches!(
            missing,
            StoreError::RevisionMismatch {
                expected: 0,
                actual: 0
            }
        ),
        "conditional save on a missing key must fail the CAS, got {missing:?}"
    );
}

/// A write premised on a stale revision is rejected and leaves the stored
/// record untouched; a write premised on the current revision lands.
pub fn cas_rejects_stale_revision(store: &dyn Store) {
    let key = ResourceKey::from("contract/cas");
    let mut record = KeyRecord::fresh(key.clone(), 1);
    store.save(record.clone(), None).unwrap();

    record.revision = 1;
    store.save(record.clone(), Some(0)).unwrap();

    record.updated_at = 99;
    let stale = store.save(record, Some(0)).unwrap_err();
    assert!(
        matches!(
            stale,
            StoreError::RevisionMismatch {
                expected: 0,
                actual: 1,
            }
        ),
        "stale write must be rejected with the actual revision, got {stale:?}"
    );
    assert_eq!(store.load(&key).unwrap().unwrap().updated_at, 1);
}

/// `keys` lists every written key, in ascending order.
pub fn keys_lists_all_recorded_keys(store: &dyn Store) {
    for name in ["contract/keys/c", "contract/keys/a", "contract/keys/b"] {
        store
            .save(KeyRecord::fresh(ResourceKey::from(name), 1), None)
            .unwrap();
    }
    let keys = store.keys().unwrap();
    for name in ["contract/keys/a", "contract/keys/b", "contract/keys/c"] {
        let key = ResourceKey::from(name);
        assert!(keys.contains(&key), "keys must list {key}; got {keys:?}");
    }
    assert!(
        keys.windows(2).all(|pair| pair[0] < pair[1]),
        "keys must be ascending, got {keys:?}"
    );
}

/// Runs the whole contract suite against `store`, which must be fresh.
pub fn run_all(store: &dyn Store) {
    load_missing_returns_none(store);
    save_then_load_round_trips(store);
    fresh_save_requires_absence(store);
    cas_rejects_stale_revision(store);
    keys_lists_all_recorded_keys(store);
}
