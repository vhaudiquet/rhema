//! The dispatcher's knobs: debounce, lease TTL, and concurrency caps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Concurrency and timing configuration for a [`crate::Dispatcher`]
/// (spec §6). All durations are milliseconds; all timing is evaluated
/// lazily against the injected clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatcherConfig {
    /// Global cap on concurrently held leases across all keys.
    pub max_concurrent_leases: u32,
    /// Per-adapter lease caps, keyed by source name. A source absent from
    /// the map is bounded only by the global cap; an entry of `0` parks that
    /// source's keys in the admission queue indefinitely (until their
    /// admission TTL cycles them, still never dropped).
    pub max_concurrent_per_source: BTreeMap<String, u32>,
    /// Coalescing window between the first event at an idle (or
    /// re-acquire) key and the moment the key becomes dispatch-eligible.
    pub debounce_window_millis: u64,
    /// Validity window of a lease grant (or renewal), from grant time.
    pub lease_ttl_millis: u64,
    /// How long a key may wait in the FIFO admission queue. On expiry the
    /// entry re-enters the normal debounce path as if its event had just
    /// arrived — queued work is re-planned against current state, never
    /// silently dropped.
    pub admission_ttl_millis: u64,
}

impl Default for DispatcherConfig {
    /// The spec's example values: three concurrent leases, a 5s debounce,
    /// a 10m lease TTL, and an admission TTL of four debounce windows.
    fn default() -> Self {
        Self {
            max_concurrent_leases: 3,
            max_concurrent_per_source: BTreeMap::new(),
            debounce_window_millis: 5_000,
            lease_ttl_millis: 600_000,
            admission_ttl_millis: 20_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_spec_examples() {
        let config = DispatcherConfig::default();
        assert_eq!(config.max_concurrent_leases, 3);
        assert_eq!(config.debounce_window_millis, 5_000);
        assert_eq!(config.lease_ttl_millis, 600_000);
        assert_eq!(
            config.admission_ttl_millis,
            4 * config.debounce_window_millis
        );
        assert!(config.max_concurrent_per_source.is_empty());
    }

    #[test]
    fn config_round_trips_through_serde() {
        let config = DispatcherConfig {
            max_concurrent_per_source: BTreeMap::from([("github".into(), 2)]),
            ..DispatcherConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: DispatcherConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }
}
