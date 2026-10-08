//! Typed errors: optimistic-concurrency conflicts, lapsed leases, and the
//! store and dispatcher error envelopes.

use crate::lease::LeaseId;
use crate::state::Revision;

/// Optimistic-concurrency rejection. `expected` is the revision the caller
/// built on; `actual` is the revision currently in the store.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Conflict {
    /// The caller's lease is stale: the key was re-acquired at a higher
    /// revision. The handler must abort rather than overwrite newer session
    /// state (same 409 shape as spec §5).
    #[error("revision mismatch: caller holds {expected}, the key is at {actual}")]
    RevisionMismatch {
        /// The revision the caller holds.
        expected: Revision,
        /// The revision currently in the store.
        actual: Revision,
    },
    /// A fresh acquirer raced a live lease. Exists because a new acquirer
    /// holds no stale revision, so [`Conflict::RevisionMismatch`] cannot
    /// express the case (documented deviation from spec §5's single
    /// variant).
    #[error("key is leased by {holder:?} (lease {lease_id})")]
    LeaseHeld {
        /// The lease currently held.
        lease_id: LeaseId,
        /// The holder recorded at acquire time.
        holder: String,
    },
}

/// A lease no longer authorizes its holder: its TTL lapsed without a
/// renewing call, or a newer lease superseded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("lease expired or was superseded (revision {revision})")]
pub struct Expired {
    /// The revision the expired lease was granted at.
    pub revision: Revision,
}

/// Why a [`crate::Store`] operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The compare-and-swap precondition on
    /// [`crate::Store::save`] did not hold; the stored record is untouched.
    #[error("store compare-and-swap failed: expected revision {expected}, found {actual}")]
    RevisionMismatch {
        /// The revision the writer assumed.
        expected: Revision,
        /// The revision actually stored.
        actual: Revision,
    },
    /// The backend failed to read or write.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Serializing or deserializing persisted state failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Everything a [`crate::Dispatcher`] method can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// Optimistic-concurrency rejection (stale revision, or the key is
    /// already leased).
    #[error(transparent)]
    Conflict(#[from] Conflict),
    /// The lease is dead: TTL lapsed or superseded.
    #[error(transparent)]
    Expired(#[from] Expired),
    /// Concurrency caps are full — globally, or for the named source. Wait
    /// and go through [`crate::Dispatcher::admit`] again.
    #[error("concurrency cap reached")]
    AtCapacity {
        /// The source whose cap is full, or `None` for the global cap.
        /// (Not named `source`: thiserror reserves that field for the
        /// wrapped error.)
        scope: Option<String>,
    },
    /// No key function is registered for the event's source.
    #[error("no key function registered for source {0:?}")]
    UnknownSource(String),
    /// The store seam failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_error_carries_its_scope() {
        let global: DispatchError = DispatchError::AtCapacity { scope: None };
        assert_eq!(global.to_string(), "concurrency cap reached");
        let per_source: DispatchError = DispatchError::AtCapacity {
            scope: Some("github".into()),
        };
        assert_eq!(per_source.to_string(), "concurrency cap reached");
        assert_eq!(
            match per_source {
                DispatchError::AtCapacity { scope } => scope,
                _ => unreachable!(),
            },
            Some("github".into())
        );
    }
}
