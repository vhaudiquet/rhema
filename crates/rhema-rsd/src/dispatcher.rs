//! The dispatcher: the per-key state machine of spec §4, the admission
//! caps of §6, and the crash recovery of §7, driven lazily against an
//! injected clock.
//!
//! Constraints this module keeps:
//!
//! - All mutations of one key's record run under a single gate mutex,
//!   load → sweep → transition → save, so every state transition is atomic
//!   and persisted before the call returns.
//! - Exclusivity and visibility are separate axes: ingest always appends to
//!   the inbox; the state machine only decides how dispatch is scheduled.
//! - Concurrency caps are enforced authoritatively in `acquire`; `admit` is
//!   advisory scheduling (it may offer a key to more than one caller, and
//!   `acquire` arbitrates).
//! - Sweeping is lazy: expired leases and expired admission entries are
//!   transitioned by whichever call touches the key — no background sweep.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::StoreError;
use crate::clock::Clock;
use crate::config::DispatcherConfig;
use crate::error::{Conflict, DispatchError, Expired};
use crate::event::{Event, InboxItem, KeyFn};
use crate::key::ResourceKey;
use crate::lease::{Lease, LeaseId};
use crate::session::Session;
use crate::state::{IngestResult, KeyState, KeyStatus, Outcome};
use crate::store::{KeyRecord, LeaseRecord, Store};

/// The resource-scoped dispatcher: dedup and key resolution on ingest, one
/// live lease per key, a live inbox per session, revision-guarded
/// checkpoints, FIFO admission with per-entry TTLs, and lazy recovery.
///
/// Shareable: every method takes `&self`, state transitions serialize on an
/// internal gate, and the whole dispatcher can be wrapped in an `Arc` and
/// called from many tasks. All methods are synchronous and quick; async
/// callers with a blocking store (SQLite) should run them on their blocking
/// pool.
pub struct Dispatcher {
    config: DispatcherConfig,
    clock: Arc<dyn Clock>,
    store: Arc<dyn Store>,
    sources: Mutex<BTreeMap<String, Arc<KeyFn>>>,
    // Serializes load → mutate → save per record; without it the revision
    // CAS would be the only line of defense between concurrent transitions.
    gate: Mutex<()>,
    leases_issued: AtomicU64,
}

impl Dispatcher {
    /// A dispatcher over `store`, reading time from `clock`.
    pub fn new(config: DispatcherConfig, clock: Arc<dyn Clock>, store: Arc<dyn Store>) -> Self {
        Self {
            config,
            clock,
            store,
            sources: Mutex::new(BTreeMap::new()),
            gate: Mutex::new(()),
            leases_issued: AtomicU64::new(1),
        }
    }

    /// Registers the pure key function for one source, replacing any prior
    /// registration. Sources register once at startup, before events flow.
    pub fn register_source(&self, source: impl Into<String>, key_fn: Arc<KeyFn>) {
        let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        sources.insert(source.into(), key_fn);
    }

    /// Records an arriving event: dedup by `(source, id)`, resolve the key
    /// through the source's key function, append to the key's inbox, then
    /// schedule per the state machine — open the debounce window at an
    /// idle key, deliver live (no state change) at a leased key, reset the
    /// debounce timer at a re-acquire key. Never dispatches.
    pub fn ingest(&self, event: Event) -> Result<IngestResult, DispatchError> {
        let key_fn = {
            let sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
            sources.get(&event.source).cloned()
        };
        let key_fn = key_fn.ok_or_else(|| DispatchError::UnknownSource(event.source.clone()))?;
        let key = key_fn(&event);
        let now = self.clock.now_millis();
        self.mutate(&key, true, |mut record| {
            let replayed = record
                .inbox
                .iter()
                .any(|item| item.event.source == event.source && item.event.id == event.id);
            if replayed {
                return Ok((record, IngestResult::Duplicate));
            }
            record.source = event.source.clone();
            record.inbox.push(InboxItem {
                event: event.clone(),
                delivered_at: now,
            });
            let result = match record.state {
                KeyState::Leased => IngestResult::DeliveredLive,
                KeyState::Idle => {
                    // The window opens on the first burst event and holds;
                    // later events of the same burst coalesce into it.
                    if record.debounce_deadline.is_none() {
                        record.debounce_deadline = Some(now + self.config.debounce_window_millis);
                    }
                    IngestResult::DebouncePending
                }
                KeyState::Reacquire => {
                    record.debounce_deadline = Some(now + self.config.debounce_window_millis);
                    IngestResult::DebouncePending
                }
            };
            Ok((record, result))
        })
    }

    /// Grants a lease on `key` (create-or-resume): sweeps an expired lease
    /// first, then atomically bumps the revision, marks the key leased, and
    /// returns the lease plus the key's persistent session — the very same
    /// session across re-acquisitions, or a fresh one on first acquire.
    ///
    /// Fails with [`Conflict::LeaseHeld`] while a live lease exists and
    /// [`DispatchError::AtCapacity`] when the global or per-source
    /// concurrency cap is full. Clears any debounce and admission markers:
    /// acquiring is the dispatch.
    pub fn acquire(
        &self,
        key: ResourceKey,
        holder: impl Into<String>,
    ) -> Result<(Lease, Session), DispatchError> {
        let now = self.clock.now_millis();
        let holder = holder.into();
        let lease_key = key.clone();
        self.mutate(&key, true, |mut record| {
            if record.state == KeyState::Leased {
                let held = record
                    .lease
                    .clone()
                    .expect("a leased record always carries its lease");
                return Err(DispatchError::Conflict(Conflict::LeaseHeld {
                    lease_id: held.id,
                    holder: held.holder,
                }));
            }
            if self.leased_count(None)? >= self.config.max_concurrent_leases as usize {
                return Err(DispatchError::AtCapacity { scope: None });
            }
            if let Some(cap) = self.config.max_concurrent_per_source.get(&record.source) {
                let used = self.leased_count(Some(&record.source))?;
                if used >= *cap as usize {
                    return Err(DispatchError::AtCapacity {
                        scope: Some(record.source.clone()),
                    });
                }
            }
            let revision = record.revision + 1;
            let id = LeaseId::new(format!(
                "lease-{}",
                self.leases_issued.fetch_add(1, Ordering::Relaxed)
            ));
            record.lease = Some(LeaseRecord {
                id: id.clone(),
                holder: holder.clone(),
                acquired_at: now,
                expires_at: now + self.config.lease_ttl_millis,
            });
            record.revision = revision;
            record.state = KeyState::Leased;
            record.debounce_deadline = None;
            record.admitted_at = None;
            let session = record
                .session
                .clone()
                .unwrap_or_else(|| Session::fresh(key.clone(), now));
            record.session = Some(session.clone());
            let lease = Lease {
                key: lease_key,
                id,
                revision,
                acquired_at: now,
                ttl_millis: self.config.lease_ttl_millis,
                holder,
            };
            Ok((record, (lease, session)))
        })
    }

    /// Proves liveness: pushes the lease deadline to `now + lease_ttl`.
    /// Fails with [`Expired`] once the TTL lapsed or a newer lease
    /// superseded this one — a lapsed lease is never resurrectable.
    pub fn renew(&self, lease: &Lease) -> Result<Lease, DispatchError> {
        let now = self.clock.now_millis();
        self.mutate(&lease.key, true, |mut record| {
            // The sweep above has already ended any lapsed lease, so a
            // matching lease here is live; a missing one means expiry (same
            // revision) or supersession (higher revision).
            let held = record
                .lease
                .as_ref()
                .is_some_and(|current| current.id == lease.id);
            if held && record.revision == lease.revision {
                let current = record.lease.as_mut().expect("held, checked above");
                current.acquired_at = now;
                current.expires_at = now + self.config.lease_ttl_millis;
                let renewed = Lease {
                    key: lease.key.clone(),
                    id: current.id.clone(),
                    revision: record.revision,
                    acquired_at: now,
                    ttl_millis: self.config.lease_ttl_millis,
                    holder: current.holder.clone(),
                };
                return Ok((record, renewed));
            }
            if record.revision == lease.revision {
                return Err(DispatchError::Expired(Expired {
                    revision: lease.revision,
                }));
            }
            Err(DispatchError::Conflict(Conflict::RevisionMismatch {
                expected: lease.revision,
                actual: record.revision,
            }))
        })
    }

    /// Drains the key's inbox: every item delivered since the caller's last
    /// poll. Only the current, unexpired lease is authorized — a lapsed or
    /// superseded lease drains nothing, so items stay for the handler that
    /// actually holds the key.
    pub fn poll_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, DispatchError> {
        self.mutate(&lease.key, true, |mut record| {
            let authorized = record
                .lease
                .as_ref()
                .is_some_and(|current| current.id == lease.id && record.revision == lease.revision);
            let items = if authorized {
                std::mem::take(&mut record.inbox)
            } else {
                Vec::new()
            };
            Ok((record, items))
        })
    }

    /// Persists `session` for the key under the revision guard: a lease
    /// whose revision no longer matches (the key was re-acquired under it)
    /// is rejected with [`Conflict::RevisionMismatch`], and a lease the
    /// sweep already ended reports [`Expired`]. The session's key and
    /// `last_active` are restamped; everything else is stored as given.
    pub fn checkpoint(&self, lease: &Lease, session: Session) -> Result<(), DispatchError> {
        let now = self.clock.now_millis();
        self.mutate(&lease.key, true, |mut record| {
            let held = record
                .lease
                .as_ref()
                .is_some_and(|current| current.id == lease.id);
            match (held, record.revision == lease.revision) {
                (true, true) => {}
                (_, false) => {
                    return Err(DispatchError::Conflict(Conflict::RevisionMismatch {
                        expected: lease.revision,
                        actual: record.revision,
                    }));
                }
                (false, true) => {
                    // Our own lease lapsed and nobody re-acquired yet.
                    return Err(DispatchError::Expired(Expired {
                        revision: lease.revision,
                    }));
                }
            }
            let mut session = session;
            session.key = record.key.clone();
            session.last_active = now;
            record.session = Some(session);
            Ok((record, ()))
        })
    }

    /// Ends the lease. An empty inbox returns the key to idle; a non-empty
    /// one (items arrived since the last poll) sends it to re-acquire with
    /// a fresh debounce window, so the same session is resumed shortly.
    ///
    /// Identity is validated but expiry is not: a handler that overslept
    /// its TTL may still clean up, as long as no newer lease superseded it.
    pub fn release(&self, lease: &Lease, outcome: Outcome) -> Result<(), DispatchError> {
        let now = self.clock.now_millis();
        // No sweep: the sweep would end the very lease being released.
        self.mutate(&lease.key, false, |mut record| {
            let held = record
                .lease
                .as_ref()
                .is_some_and(|current| current.id == lease.id)
                && record.revision == lease.revision;
            if !held {
                return Err(DispatchError::Conflict(Conflict::RevisionMismatch {
                    expected: lease.revision,
                    actual: record.revision,
                }));
            }
            end_lease(&mut record, now, &self.config, Some(outcome));
            Ok((record, ()))
        })
    }

    /// The lazy dispatch step. Sweeps expired leases and admission entries,
    /// then returns the keys that are due — debounce elapsed with pending
    /// inbox items — up to the free global and per-source slots, in FIFO
    /// order of becoming due. Keys that stay blocked enter (or remain in)
    /// the FIFO admission queue, from which they re-enter the debounce path
    /// when their admission TTL lapses — queued work is never dropped.
    ///
    /// Advisory: acquire enforces the caps, so a key offered here may still
    /// report [`DispatchError::AtCapacity`] if another caller took the slot.
    pub fn admit(&self) -> Result<Vec<ResourceKey>, DispatchError> {
        let now = self.clock.now_millis();
        self.sweep_all()?;

        let mut records = Vec::new();
        for key in self.store.keys()? {
            if let Some(record) = self.store.load(&key)? {
                records.push(record);
            }
        }
        let leased_total = records
            .iter()
            .filter(|record| record.state == KeyState::Leased)
            .count();
        let mut used_per_source: BTreeMap<String, usize> = BTreeMap::new();
        for record in &records {
            if record.state == KeyState::Leased {
                *used_per_source.entry(record.source.clone()).or_default() += 1;
            }
        }

        let mut due: Vec<&KeyRecord> = records
            .iter()
            .filter(|record| {
                record.state != KeyState::Leased
                    && !record.inbox.is_empty()
                    && record
                        .debounce_deadline
                        .is_some_and(|deadline| deadline <= now)
            })
            .collect();
        // FIFO by becoming due; the key breaks ties deterministically.
        due.sort_by(|a, b| {
            a.debounce_deadline
                .cmp(&b.debounce_deadline)
                .then_with(|| a.key.cmp(&b.key))
        });

        let mut free_global =
            (self.config.max_concurrent_leases as usize).saturating_sub(leased_total);
        let mut admitted = Vec::new();
        for record in due {
            let source_free = match self.config.max_concurrent_per_source.get(&record.source) {
                Some(cap) => (*cap as usize)
                    .saturating_sub(used_per_source.get(&record.source).copied().unwrap_or(0)),
                None => usize::MAX,
            };
            if free_global > 0 && source_free > 0 {
                free_global -= 1;
                *used_per_source.entry(record.source.clone()).or_default() += 1;
                if record.admitted_at.is_some() {
                    let key = record.key.clone();
                    self.mutate(&key, false, |mut r| {
                        r.admitted_at = None;
                        Ok((r, ()))
                    })?;
                }
                admitted.push(record.key.clone());
            } else if record.admitted_at.is_none() {
                let key = record.key.clone();
                let enqueued_at = now;
                self.mutate(&key, false, |mut r| {
                    r.admitted_at = Some(enqueued_at);
                    Ok((r, ()))
                })?;
            }
        }
        Ok(admitted)
    }

    /// Startup sweep: resets every abandoned key — a lease past its TTL
    /// goes back to idle, or to re-acquire with a fresh debounce window
    /// when its inbox holds undelivered items — and expires admission
    /// entries back into the debounce path. Idempotent; the owner calls it
    /// once before ingesting (or any time it wants a full sweep).
    pub fn recover(&self) -> Result<(), DispatchError> {
        self.sweep_all()
    }

    /// The stored state of one key. A key never seen reports idle at
    /// revision 0.
    pub fn status(&self, key: &ResourceKey) -> Result<KeyStatus, DispatchError> {
        let status = match self.store.load(key)? {
            Some(record) => KeyStatus::from_record(&record),
            None => KeyStatus::missing(key.clone()),
        };
        Ok(status)
    }

    /// Status of every currently leased key, in ascending key order.
    /// A pure read: it does not sweep, so a lapsed-but-unswept lease still
    /// shows as leased until the next `admit`/`recover`/per-key call.
    pub fn list_active(&self) -> Result<Vec<KeyStatus>, DispatchError> {
        let mut active = Vec::new();
        for key in self.store.keys()? {
            if let Some(record) = self.store.load(&key)? {
                if record.state == KeyState::Leased {
                    active.push(KeyStatus::from_record(&record));
                }
            }
        }
        Ok(active)
    }

    /// Runs one sweep pass over every key, persisting whichever records the
    /// sweep moved.
    fn sweep_all(&self) -> Result<(), DispatchError> {
        for key in self.store.keys()? {
            self.mutate(&key, true, |record| Ok((record, ())))?;
        }
        Ok(())
    }

    /// How many records currently hold a lease, optionally narrowed to one
    /// source. Must be called under the gate.
    fn leased_count(&self, source: Option<&str>) -> Result<usize, StoreError> {
        let mut leased = 0;
        for key in self.store.keys()? {
            if let Some(record) = self.store.load(&key)? {
                if record.state == KeyState::Leased && source.is_none_or(|s| record.source == s) {
                    leased += 1;
                }
            }
        }
        Ok(leased)
    }

    /// Applies `f` to the record for `key` under the gate: loads (or
    /// starts from a fresh record), optionally sweeps, applies, and saves
    /// only when the record actually changed — stamped and CAS-guarded on
    /// the revision that was read. Errors from `f` persist nothing.
    fn mutate<T>(
        &self,
        key: &ResourceKey,
        sweep: bool,
        f: impl FnOnce(KeyRecord) -> Result<(KeyRecord, T), DispatchError>,
    ) -> Result<T, DispatchError> {
        let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let loaded = self.store.load(key)?;
        let now = self.clock.now_millis();
        let blank = KeyRecord::fresh(key.clone(), now);
        let next = match &loaded {
            Some(record) => {
                let mut record = record.clone();
                if sweep {
                    record = sweep_record(record, now, &self.config);
                }
                record
            }
            None => blank.clone(),
        };
        let (mut record, output) = f(next)?;
        let changed = match &loaded {
            Some(previous) => *previous != record,
            None => record != blank,
        };
        if changed {
            let expected = loaded.as_ref().map(|previous| previous.revision);
            record.updated_at = now;
            self.store.save(record, expected)?;
        }
        Ok(output)
    }
}

/// Ends a lease: idle on an empty inbox, re-acquire with a fresh debounce
/// window when items are still waiting for the next handler.
fn end_lease(
    record: &mut KeyRecord,
    now: u64,
    config: &DispatcherConfig,
    outcome: Option<Outcome>,
) {
    record.lease = None;
    if record.inbox.is_empty() {
        record.state = KeyState::Idle;
        record.debounce_deadline = None;
    } else {
        record.state = KeyState::Reacquire;
        record.debounce_deadline = Some(now + config.debounce_window_millis);
    }
    if let Some(outcome) = outcome {
        record.last_outcome = Some(outcome);
    }
}

/// Lazily evaluates every deadline on one record at `now`:
///
/// - a lease past its TTL ends (idle, or re-acquire on a non-empty inbox);
/// - an admission entry past its TTL leaves the queue and re-enters the
///   normal debounce path, as if its event had just arrived.
fn sweep_record(mut record: KeyRecord, now: u64, config: &DispatcherConfig) -> KeyRecord {
    let lapsed = record
        .lease
        .as_ref()
        .is_some_and(|lease| lease.expires_at <= now);
    if lapsed && record.state == KeyState::Leased {
        end_lease(&mut record, now, config, None);
    }
    if record.state != KeyState::Leased {
        let expired = record
            .admitted_at
            .is_some_and(|enqueued| now >= enqueued.saturating_add(config.admission_ttl_millis));
        if expired {
            record.admitted_at = None;
            record.debounce_deadline = Some(now + config.debounce_window_millis);
        }
    }
    record
}
