//! The event hub: per-key broadcast of live turn events.
//!
//! HTTP subscribers stream one key's turns through it: a chat UI holds
//! one `GET /v1/sessions/{key}/events` connection open and watches every
//! turn of that session go by — whoever triggered the turn. The hub is a
//! [`tokio::sync::broadcast`] channel per key behind a mutex'd map;
//! senders live for the process lifetime (one small `Sender` per key),
//! and a channel with no receivers is pruned on the next emit.
//!
//! Delivery is best-effort by construction: a subscriber that falls more
//! than [`EventHub::capacity`] events behind drops the oldest ones
//! (`broadcast`'s contract) — a live stream of a running turn may skip,
//! and the receiver surfaces the skip as a `Lagged` error so clients can
//! say so. The authoritative record of a turn is the session, never the
//! stream.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use rhema_rsd::ResourceKey;
use serde::Serialize;
use tokio::sync::broadcast;

/// How many events a subscriber may trail by before the oldest drop.
/// Generous: a turn's deltas arrive faster than any viewer, but rarely
/// past four digits.
pub const TURN_CHANNEL_CAPACITY: usize = 1024;

/// One event of a turn, as the wire sees it. Serialized with an internal
/// `type` tag, snake-cased: `{"type":"text_delta","text":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// A piece of the model's reasoning trace, in stream order.
    ThinkingDelta {
        /// The text fragment.
        text: String,
    },
    /// A piece of the model's reply, in stream order.
    TextDelta {
        /// The text fragment.
        text: String,
    },
    /// A tool call begins.
    ToolStart {
        /// The tool's name.
        name: String,
    },
    /// A tool result landed.
    ToolResult {
        /// The tool's name.
        name: String,
        /// Whether the call failed.
        is_error: bool,
        /// A bounded snippet of the result content.
        summary: String,
    },
    /// The turn is over — the clean end marker subscribers wait on,
    /// emitted by the handler itself, so it arrives whoever triggered
    /// the turn.
    TurnDone {
        /// The turn's final assistant text, when it produced one.
        reply: Option<String>,
        /// `handled` or `failed`.
        outcome: String,
    },
}

/// The per-key broadcast of [`TurnEvent`]s.
pub struct EventHub {
    channels: Mutex<HashMap<ResourceKey, broadcast::Sender<TurnEvent>>>,
    capacity: usize,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::with_capacity(TURN_CHANNEL_CAPACITY)
    }
}

impl EventHub {
    /// A hub whose per-key channels hold `capacity` events.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Subscribes to `key`'s turns, creating the channel on first use.
    /// Subscribe BEFORE triggering work: a turn that starts before the
    /// subscriber exists is simply not streamed to it.
    pub fn subscribe(&self, key: &ResourceKey) -> broadcast::Receiver<TurnEvent> {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        match channels.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(occupied) => occupied.get().subscribe(),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let (sender, receiver) = broadcast::channel(self.capacity);
                vacant.insert(sender);
                receiver
            }
        }
    }

    /// Publishes one event to `key`'s subscribers. A key with nobody
    /// listening is not an error — turns run whether or not anyone
    /// watches.
    pub fn emit(&self, key: &ResourceKey, event: TurnEvent) {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sender) = channels.get(key) else {
            return;
        };
        // `send` fails only when no receiver holds the channel; that is
        // the "nobody watching" case, not a failure. Afterwards the
        // channel has no reason to keep existing — the next subscriber
        // recreates it.
        if sender.send(event).is_err() && sender.receiver_count() == 0 {
            channels.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> ResourceKey {
        ResourceKey::from(format!("interactive:{name}"))
    }

    #[test]
    fn events_serialize_with_a_type_tag_and_snake_case() {
        assert_eq!(
            serde_json::to_string(&TurnEvent::TextDelta { text: "hi".into() }).unwrap(),
            r#"{"type":"text_delta","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&TurnEvent::ToolResult {
                name: "bash".into(),
                is_error: true,
                summary: "exit 1".into()
            })
            .unwrap(),
            r#"{"type":"tool_result","name":"bash","is_error":true,"summary":"exit 1"}"#
        );
        assert_eq!(
            serde_json::to_string(&TurnEvent::TurnDone {
                reply: Some("done".into()),
                outcome: "handled".into()
            })
            .unwrap(),
            r#"{"type":"turn_done","reply":"done","outcome":"handled"}"#
        );
    }

    #[test]
    fn a_subscriber_receives_what_is_emitted_after_it_subscribed() {
        let hub = EventHub::default();
        let key = key("web");
        let mut receiver = hub.subscribe(&key);
        hub.emit(
            &key,
            TurnEvent::ToolStart {
                name: "bash".into(),
            },
        );
        hub.emit(
            &key,
            TurnEvent::TurnDone {
                reply: None,
                outcome: "handled".into(),
            },
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::ToolStart {
                name: "bash".into()
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::TurnDone {
                reply: None,
                outcome: "handled".into()
            }
        );
    }

    #[test]
    fn emitting_to_nobody_listening_is_quiet_and_prunes_the_channel() {
        let hub = EventHub::default();
        let key = key("alone");
        // No subscriber ever: quiet, and nothing accumulates.
        hub.emit(&key, TurnEvent::TextDelta { text: "x".into() });
        assert_eq!(hub.channels.lock().unwrap().len(), 0);

        // A subscriber that leaves mid-turn: the next emit prunes.
        let receiver = hub.subscribe(&key);
        drop(receiver);
        hub.emit(&key, TurnEvent::TextDelta { text: "x".into() });
        assert_eq!(hub.channels.lock().unwrap().len(), 0);
    }

    #[test]
    fn keys_do_not_hear_each_other() {
        let hub = EventHub::default();
        let mut mine = hub.subscribe(&key("mine"));
        hub.subscribe(&key("theirs"));
        hub.emit(&key("theirs"), TurnEvent::TextDelta { text: "x".into() });
        assert!(mine.try_recv().is_err(), "no cross-talk");
    }

    #[test]
    fn a_lagging_subscriber_drops_the_oldest_and_is_told_so() {
        let hub = EventHub::with_capacity(2);
        let key = key("slow");
        let mut receiver = hub.subscribe(&key);
        for n in 1..=4 {
            hub.emit(
                &key,
                TurnEvent::TextDelta {
                    text: n.to_string(),
                },
            );
        }
        assert!(
            matches!(
                receiver.try_recv(),
                Err(broadcast::error::TryRecvError::Lagged(2))
            ),
            "the skip is surfaced, not silent"
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::TextDelta { text: "3".into() },
            "the newest capacity events survive"
        );
    }
}
