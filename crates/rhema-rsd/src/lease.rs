//! Leases: the time-bounded claim that makes a key single-handler.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::key::ResourceKey;
use crate::state::Revision;

/// Identity of one lease grant, unique per [`crate::Dispatcher`] process.
/// Leases do not survive a restart — recovery sweeps them instead — so a
/// process-local counter is a sufficient uniqueness source.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(String);

impl LeaseId {
    /// Wraps `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for LeaseId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for LeaseId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A live, renewable claim on one [`ResourceKey`], held by one handler.
///
/// The lease is valid from `acquired_at` through `acquired_at + ttl_millis`
/// (see [`Lease::expires_at`]). Handlers renew to push the deadline forward;
/// past the deadline the lease no longer authorizes polling, checkpointing,
/// or renewing, and the key is swept back to idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The key this lease claims.
    pub key: ResourceKey,
    /// Identity of this grant.
    pub id: LeaseId,
    /// The key revision this lease was granted at; bumped on every
    /// acquisition. Guards checkpoint/release against stale holders.
    pub revision: Revision,
    /// Grant (or last renewal) time in milliseconds since the Unix epoch.
    pub acquired_at: u64,
    /// Validity window from `acquired_at`, in milliseconds.
    pub ttl_millis: u64,
    /// Handler identity supplied at acquire time, for ops and diagnostics.
    pub holder: String,
}

impl Lease {
    /// The deadline in milliseconds since the Unix epoch: the first
    /// millisecond at which the lease no longer authorizes its holder.
    pub fn expires_at(&self) -> u64 {
        self.acquired_at.saturating_add(self.ttl_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_the_window_from_the_last_grant() {
        let lease = Lease {
            key: ResourceKey::from("github:acme/widgets#142"),
            id: LeaseId::from("lease-1"),
            revision: 3,
            acquired_at: 1_000,
            ttl_millis: 10_000,
            holder: "handler-1".into(),
        };
        assert_eq!(lease.expires_at(), 11_000);
        let json = serde_json::to_string(&lease).unwrap();
        let back: Lease = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lease);
    }
}
