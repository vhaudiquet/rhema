//! Events and their live delivery: what a source produces and what a
//! handler's inbox holds.

use serde::{Deserialize, Serialize};

use crate::key::ResourceKey;

/// One unit of "something happened", produced by a source (a poller or a
/// webhook receiver).
///
/// `id` is the dedup id within `source` — a webhook delivery id, a
/// notification thread id. `payload` is JSON so the event round-trips
/// through the store with the rest of the dispatch state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Dedup id, unique within [`Event::source`].
    pub id: String,
    /// Which adapter produced the event: `"github"`, `"gitea"`, ...
    pub source: String,
    /// The event body. The key function reads the resource identity from
    /// here; handlers read whatever else they need.
    pub payload: serde_json::Value,
    /// Arrival time in milliseconds since the Unix epoch, stamped by the
    /// source.
    pub received_at: u64,
}

/// One delivered event sitting in a key's inbox until a lease drains it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    /// The event that was ingested.
    pub event: Event,
    /// Ingest time in milliseconds since the Unix epoch.
    pub delivered_at: u64,
}

/// Adapter-supplied, pure mapping from an event to its resource key.
///
/// Registered per source with [`crate::Dispatcher::register_source`]; the
/// dispatcher never inspects payloads itself.
pub type KeyFn = dyn Fn(&Event) -> ResourceKey + Send + Sync;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbox_item_round_trips_through_serde() {
        let item = InboxItem {
            event: Event {
                id: "delivery-1".into(),
                source: "github".into(),
                payload: serde_json::json!({ "action": "commented" }),
                received_at: 1728200000000,
            },
            delivered_at: 1728200000001,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: InboxItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, item);
    }
}
