//! Per-key state vocabulary: the state machine states, the revision
//! counter, ingest results, release outcomes, and the ops-facing status
//! report.

use serde::{Deserialize, Serialize};

use crate::key::ResourceKey;
use crate::lease::Lease;

/// Monotonically increasing counter per resource key, bumped on every lease
/// acquisition. Guards session writes: a handler holding revision N is
/// rejected once the key sits at N+1.
pub type Revision = u64;

/// The three states a resource key is in, exactly one at a time.
///
/// `RE-ACQUIRE` in the spec is spelled [`KeyState::Reacquire`] here; the
/// serialized form is `"reacquire"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    /// No lease held and no dispatch pending.
    #[default]
    Idle,
    /// A live lease is held by a handler.
    Leased,
    /// The previous lease ended with undelivered inbox items; a debounce
    /// window runs before the next dispatch resumes the same session.
    Reacquire,
}

impl std::fmt::Display for KeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            KeyState::Idle => "idle",
            KeyState::Leased => "leased",
            KeyState::Reacquire => "re-acquire",
        };
        f.write_str(name)
    }
}

/// What a handler reports when it releases a lease. Recorded on the key for
/// introspection; the dispatcher never branches on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The handler finished its work.
    Handled,
    /// The handler failed; the payload says why.
    Failed(String),
    /// The work is no longer wanted (event superseded, task cancelled).
    Superseded,
}

/// What [`crate::Dispatcher::ingest`] did with an event.
///
/// The spec's `Dispatched(Lease, Session)` does not exist here: dispatch is
/// lazy (no timers), so `ingest` records and reports, and the driver later
/// calls [`crate::Dispatcher::admit`] plus `acquire`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestResult {
    /// The key is leased: the event was appended to the live inbox and the
    /// running handler will see it at its next poll.
    DeliveredLive,
    /// The key is idle or awaiting re-acquire: the event was recorded and
    /// the debounce window (re)opened; the driver admits the key later.
    DebouncePending,
    /// An event with the same `(source, id)` is still pending for the key;
    /// the replay was dropped. Delivery is at-least-once across drains.
    Duplicate,
}

/// Introspection report for one resource key, as returned by
/// [`crate::Dispatcher::status`] and [`crate::Dispatcher::list_active`].
/// Derived view, never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyStatus {
    /// The reported key.
    pub key: ResourceKey,
    /// Current state machine state.
    pub state: KeyState,
    /// Current revision.
    pub revision: Revision,
    /// The live lease, when state is [`KeyState::Leased`].
    pub lease: Option<Lease>,
    /// Undelivered inbox items waiting on this key.
    pub inbox_depth: usize,
    /// When the debounce window ends, for idle/re-acquire keys with pending
    /// work.
    pub debounce_deadline: Option<u64>,
    /// When the key entered the admission queue, while it waits there.
    pub admitted_at: Option<u64>,
    /// The outcome recorded by the most recent release.
    pub last_outcome: Option<Outcome>,
    /// Last mutation of the underlying record.
    pub updated_at: u64,
}

impl KeyStatus {
    pub(crate) fn from_record(record: &crate::store::KeyRecord) -> Self {
        let lease = record.lease.as_ref().map(|lease| Lease {
            key: record.key.clone(),
            id: lease.id.clone(),
            revision: record.revision,
            acquired_at: lease.acquired_at,
            // Saturating: a hand-corrupted row with expires_at below
            // acquired_at must not panic the status read.
            ttl_millis: lease.expires_at.saturating_sub(lease.acquired_at),
            holder: lease.holder.clone(),
        });
        Self {
            key: record.key.clone(),
            state: record.state,
            revision: record.revision,
            lease,
            inbox_depth: record.inbox.len(),
            debounce_deadline: record.debounce_deadline,
            admitted_at: record.admitted_at,
            last_outcome: record.last_outcome.clone(),
            updated_at: record.updated_at,
        }
    }

    pub(crate) fn missing(key: ResourceKey) -> Self {
        Self {
            key,
            state: KeyState::Idle,
            revision: 0,
            lease: None,
            inbox_depth: 0,
            debounce_deadline: None,
            admitted_at: None,
            last_outcome: None,
            updated_at: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_serialize_as_snake_case_tags() {
        assert_eq!(serde_json::to_string(&KeyState::Idle).unwrap(), r#""idle""#);
        assert_eq!(
            serde_json::to_string(&KeyState::Reacquire).unwrap(),
            r#""reacquire""#
        );
        assert_eq!(KeyState::Idle.to_string(), "idle");
        assert_eq!(KeyState::Reacquire.to_string(), "re-acquire");
    }

    #[test]
    fn outcomes_round_trip() {
        let outcome = Outcome::Failed("tests red".into());
        let json = serde_json::to_string(&outcome).unwrap();
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, outcome);
    }
}
