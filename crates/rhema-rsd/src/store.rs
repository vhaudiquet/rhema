//! The persistence seam: one durable record per resource key.
//!
//! The dispatcher reads and writes whole [`KeyRecord`]s through [`Store`].
//! The trait is deliberately small so the bundled [`crate::MemoryStore`] and
//! the durable [`crate::SqliteStore`] share one behavioral contract,
//! replayed by the `store-contract` feature's [`crate::store_contract`]
//! suite.

use serde::{Deserialize, Serialize};

use crate::event::InboxItem;
use crate::key::ResourceKey;
use crate::lease::LeaseId;
use crate::session::Session;
use crate::state::{KeyState, Outcome, Revision};

/// The lease grant as persisted on a key record. Unlike the public
/// [`crate::Lease`] view it carries the deadline directly and not the
/// revision (which the record holds once).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// Identity of the grant.
    pub id: LeaseId,
    /// Handler identity supplied at acquire time.
    pub holder: String,
    /// Grant or last renewal, in milliseconds since the Unix epoch.
    pub acquired_at: u64,
    /// Deadline after which the lease is swept, in the same units.
    pub expires_at: u64,
}

/// Everything the dispatcher persists about one resource key: the state
/// machine state, the revision, the live lease, the pending inbox, the
/// debounce and admission markers, and the session itself.
///
/// This is the row shape of spec §7 (`queue` is [`KeyRecord::inbox`]); the
/// `admitted_at` marker realizes the FIFO admission queue as a per-key
/// enqueue timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyRecord {
    /// The key this record belongs to.
    pub key: ResourceKey,
    /// State machine state.
    pub state: KeyState,
    /// Bumped on every lease acquisition; the compare-and-swap token of
    /// every store write.
    pub revision: Revision,
    /// The live lease, exactly when `state` is [`KeyState::Leased`].
    pub lease: Option<LeaseRecord>,
    /// Pending inbox items in delivery order; the spec's ordered event
    /// queue. Drained by the lease holder's poll.
    pub inbox: Vec<InboxItem>,
    /// Source of the most recent ingested event; per-source admission caps
    /// are accounted against it.
    pub source: String,
    /// The persistent session, present from the first acquire on.
    pub session: Option<Session>,
    /// When the debounce window ends, while one is open.
    pub debounce_deadline: Option<u64>,
    /// When the key entered the admission queue; `None` while it is not
    /// queued. Entries expire back into the debounce path after the
    /// configured admission TTL.
    pub admitted_at: Option<u64>,
    /// The outcome recorded by the most recent release.
    pub last_outcome: Option<Outcome>,
    /// Last mutation, in milliseconds since the Unix epoch.
    pub updated_at: u64,
}

impl KeyRecord {
    /// The record a key starts from: idle, revision 0, nothing pending.
    pub fn fresh(key: ResourceKey, now_millis: u64) -> Self {
        Self {
            key,
            state: KeyState::Idle,
            revision: 0,
            lease: None,
            inbox: Vec::new(),
            source: String::new(),
            session: None,
            debounce_deadline: None,
            admitted_at: None,
            last_outcome: None,
            updated_at: now_millis,
        }
    }
}

/// Durable dispatch state for one [`ResourceKey`] each.
///
/// # Contract
///
/// Every implementation must hold the following invariants; the
/// `store-contract` feature's [`crate::store_contract`] suite replays them
/// as tests, so a new backend can prove itself against the exact suite the
/// in-memory store passes.
///
/// - **Whole-record atomicity.** [`Store::save`] replaces the entire record
///   for its key atomically: a reader observes either the old or the new
///   record, never a mix.
/// - **Compare-and-swap on the revision.** `save` succeeds only when the
///   persisted record's [`KeyRecord::revision`] equals `expected_revision`,
///   or when `expected_revision` is `None` and no record exists for the key.
///   Every other case fails with [`crate::StoreError::RevisionMismatch`] and
///   leaves the stored record untouched — concurrent dispatchers cannot
///   lose updates.
/// - **Ordered queue.** The record's inbox is stored and returned in its
///   given order; FIFO event delivery depends on it.
/// - **Faithful round-trip.** Loading a saved record yields an equal record:
///   every field, including session history and JSON payloads, survives.
/// - **Durability before return.** When `save` returns `Ok`, the mutation is
///   already durable (for an embedded database: committed). The dispatcher
///   persists every state transition before its own call returns, so a crash
///   between calls never loses coordination state.
/// - **Deterministic enumeration.** [`Store::keys`] returns every key with a
///   record, in ascending order.
pub trait Store: Send + Sync {
    /// The record for `key`, or `None` when the key was never written.
    fn load(&self, key: &ResourceKey) -> Result<Option<KeyRecord>, crate::StoreError>;

    /// Atomically persist `record` under the revision precondition
    /// described above. `expected_revision` is the revision the writer
    /// read (`None` = the key must not exist yet).
    fn save(
        &self,
        record: KeyRecord,
        expected_revision: Option<Revision>,
    ) -> Result<(), crate::StoreError>;

    /// Every key with a record, in ascending order.
    fn keys(&self) -> Result<Vec<ResourceKey>, crate::StoreError>;
}
