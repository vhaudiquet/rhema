//! rhema-rsd — resource-scoped dispatch: the reference implementation of the
//! RSD `SessionManager` contract (specs/resource-scoped-dispatch.md §10).
//!
//! External events name a [`ResourceKey`] — an issue, a PR, a thread — and
//! the [`Dispatcher`] turns them into agent work with four guarantees:
//!
//! - **At most one live handler per key.** [`Dispatcher::acquire`] grants a
//!   time-bounded, renewable [`Lease`]; a second acquire on the same key is
//!   rejected until release, and a lapsed lease is swept back.
//! - **No event silently dropped.** Every ingested [`Event`] lands in the
//!   key's inbox immediately, whether or not a lease is held; a running
//!   handler sees it at its next [`Dispatcher::poll_inbox`] call.
//! - **Session continuity.** Each key carries one persistent [`Session`] —
//!   wrapping [`rhema_core::Session`] (history + driver state) plus plan,
//!   artifact reference, and timestamps — resumed by every re-acquisition
//!   and checkpointed under a revision guard.
//! - **No stuck keys, bounded parallelism.** Leases carry a TTL guarded by
//!   [`Dispatcher::renew`]; abandoned leases are swept back to idle by
//!   [`Dispatcher::recover`] and [`Dispatcher::admit`]. Global and
//!   per-source concurrency caps feed a FIFO admission queue whose entries
//!   expire back into the debounce path — queued work is re-planned, never
//!   dropped.
//!
//! # The driver owns the loop
//!
//! This crate has no threads, tasks, or timers. All time-dependent behavior
//! (debounce windows, lease TTL, admission TTL, recovery sweeps) is
//! evaluated lazily against an injected [`Clock`] at call time, so the crate
//! stays embeddable and every timing is deterministic in tests
//! ([`FixedClock`]). The embedder drives the loop:
//!
//! ```text
//! dispatcher.register_source("github", key_fn);
//! dispatcher.recover()?;                      // once, at startup
//! loop {
//!     for event in poll_external() { dispatcher.ingest(event)?; }
//!     for key in dispatcher.admit()? {        // debounce elapsed, caps allow
//!         spawn handler {
//!             let (lease, mut session) = dispatcher.acquire(key, holder)?;
//!             loop {
//!                 for item in dispatcher.poll_inbox(&lease)? { fold(&mut session, item); }
//!                 ... one reasoning step ...
//!                 dispatcher.checkpoint(&lease, session.clone())?;
//!                 if done { break; }
//!             }
//!             dispatcher.release(&lease, outcome)?;
//!         }
//!     }
//!     wait_for_events_or_time();
//! }
//! ```
//!
//! `ingest` never dispatches: it records the event, opens (or resets) the
//! debounce window, and returns. `admit` is the lazy dispatch step — it
//! sweeps expired leases, expires stale admission entries back into the
//! debounce path, and returns the keys that may now be acquired. Caps are
//! enforced authoritatively in `acquire`; `admit` is the scheduler that
//! decides when a key is offered, so concurrent `admit` calls may offer the
//! same key twice — the losing `acquire` simply reports the conflict.
//!
//! # Deviations from spec §5
//!
//! The spec sketches a dispatcher that dispatches synchronously from
//! `ingest`. Without timers that is impossible in a thread-free crate, so
//! dispatch is lazy; everything else keeps the spec's names and semantics.
//! The full list, each also noted at the item it concerns:
//!
//! - `IngestResult::Dispatched(Lease, Session)` is replaced by
//!   [`IngestResult::DebouncePending`]: `ingest` records and opens the
//!   debounce window; the driver later calls `admit` + `acquire`. A
//!   [`IngestResult::Duplicate`] variant is added for replayed event ids.
//! - `ingest` drops the redundant `source` parameter — [`Event::source`] is
//!   authoritative.
//! - `register_source` omits `classify_fn`: per §9 classification is
//!   adapter-specific, the mechanism never reads it, and the spec never
//!   defines the `Classification` type.
//! - `acquire` takes a `holder` argument, populating §3's `Lease.holder`.
//! - [`Conflict`] gains a `LeaseHeld` variant: a fresh acquirer holds no
//!   stale revision, so `RevisionMismatch` alone cannot express
//!   acquire-on-leased contention.
//! - `status`/`list_active` return a [`KeyStatus`] report rather than the
//!   bare [`KeyState`] enum (§5 reuses `KeyState` for both the state and the
//!   introspection return; here the state keeps the name and the report gets
//!   its own).
//! - `poll_inbox` drains by removal: the inbox is the pending-delivery queue
//!   of spec §7 (`queue` column), and the "per-lease cursor" is realized by
//!   authorizing only the current, unexpired lease — a stale lease observes
//!   nothing. It returns `Result` because it goes through the fallible
//!   [`Store`] seam, as do `status` and `list_active`.
//! - `renew`'s `Expired` failure also covers a superseded lease (stolen by a
//!   newer revision), since the spec gives `renew` no other error type.
//! - Dedup by `(source, event.id)` scopes to the key's *pending* inbox
//!   items: delivery is at-least-once across drain boundaries.
//! - Timestamps are milliseconds since the Unix epoch (`u64`) read from the
//!   [`Clock`]; durations are milliseconds. Payloads are
//!   `serde_json::Value` rather than raw bytes, so everything round-trips
//!   through serde for checkpointing.
//!
//! # The store seam
//!
//! Dispatch state lives behind the [`Store`] trait: whole-record
//! persistence with a compare-and-swap on the revision, ordered inbox,
//! and durability before `save` returns. The bundled [`VolatileStore`] backs
//! tests and in-process use; [`SqliteStore`] is the durable reference
//! backend (spec §7, the default-on `sqlite` feature), and both replay
//! the exact behavioral suite via the `store-contract` feature's
//! [`store_contract`] module.
//!
//! Layout:
//! - [`clock`] — the `Clock` seam, `SystemClock`, `FixedClock`
//! - [`key`] — `ResourceKey`
//! - [`event`] — `Event`, `InboxItem`, the `KeyFn` alias
//! - [`lease`] — `LeaseId`, `Lease`
//! - [`session`] — the persistent per-key `Session` (wraps `rhema_core::Session`)
//! - [`state`] — `Revision`, `KeyState`, `KeyStatus`, `Outcome`, `IngestResult`
//! - [`error`] — `Conflict`, `Expired`, `StoreError`, `DispatchError`
//! - [`config`] — `DispatcherConfig`
//! - [`store`] — the `Store` seam, `KeyRecord`, `LeaseRecord`
//! - [`volatile`] — the in-process `VolatileStore`
//! - [`sqlite`] — the durable `SqliteStore` (`sqlite` feature)
//! - [`dispatcher`] — the `Dispatcher` state machine
//! - [`store_contract`] — replayable Store behavioral suite (`store-contract` feature)

#![forbid(unsafe_code)]

pub mod clock;
pub mod config;
pub mod dispatcher;
pub mod error;
pub mod event;
pub mod key;
pub mod lease;
pub mod session;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod state;
pub mod store;
#[cfg(feature = "store-contract")]
pub mod store_contract;
pub mod volatile;

pub use clock::{Clock, FixedClock, SystemClock};
pub use config::DispatcherConfig;
pub use dispatcher::Dispatcher;
pub use error::{Conflict, DispatchError, Expired, StoreError};
pub use event::{Event, InboxItem, KeyFn};
pub use key::ResourceKey;
pub use lease::{Lease, LeaseId};
pub use session::Session;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;
pub use state::{IngestResult, KeyState, KeyStatus, Outcome, Revision};
pub use store::{KeyRecord, LeaseRecord, Store};
pub use volatile::VolatileStore;
