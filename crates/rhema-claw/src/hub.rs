//! The event hub: per-key broadcast of live turn events, and the
//! per-key record a late subscriber replays.
//!
//! HTTP subscribers stream one key's turns through it: a chat UI holds
//! one `GET /v1/sessions/{key}/events` connection open and watches every
//! turn of that session go by — whoever triggered the turn. The hub is
//! a [`tokio::sync::broadcast`] channel per key behind a mutex'd map,
//! and each entry carries the key's recorded events beside the channel.
//! `emit` appends to the record and broadcasts in one lock; a
//! subscriber that asks for history gets a record snapshot and a live
//! receiver cut under that same lock — every event lands in exactly one
//! of the two, so a replay has no gap and no overlap with the live
//! stream, ever.
//!
//! Two bounds keep the record honest rather than infinite: it holds at
//! most [`RECORD_CAPACITY`] events per key (the oldest fall off), and
//! when a record store is attached, each turn's end marker flushes the
//! key's record there — so the history a restart replays is the history
//! the completed turns streamed. Live delivery itself stays
//! best-effort: a subscriber that falls more than
//! [`EventHub::capacity`] events behind drops the oldest ones
//! (`broadcast`'s contract) and the receiver surfaces the skip as a
//! `Lagged` error so clients can say so.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use rhema_rsd::ResourceKey;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::turn_store::TurnStore;

/// How many events a subscriber may trail by before the oldest drop.
/// Generous: a turn's deltas arrive faster than any viewer, but rarely
/// past four digits.
pub const TURN_CHANNEL_CAPACITY: usize = 1024;

/// How many events one key's record holds. Wide enough that a reload
/// rebuilds whole recent turns — thinking traces and tool rows are
/// deltas, and they add up — narrow enough that a flush stays one
/// modest UPSERT.
pub const RECORD_CAPACITY: usize = 8192;

/// One event of a turn, as the wire sees it. Serialized with an internal
/// `type` tag, snake-cased: `{"type":"text_delta","text":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// The turn's input: the text (a batch folds into one) the turn
    /// answers, recorded by the handler when the dispatch starts. It
    /// exists for the replay — a chat surface shows its own messages
    /// as it sends them, and skips the live echo.
    UserMessage {
        /// The input text, verbatim.
        text: String,
    },
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
        /// The process exit code of command-like tools; `None` otherwise.
        code: Option<i32>,
        /// The command line a shell tool ran, parsed from the call's
        /// streamed arguments; `None` for every other tool.
        command: Option<String>,
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

impl TurnEvent {
    /// The event's kind, spelled the way the wire's `type` tag spells
    /// it — for logs that name what they dropped.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => "user_message",
            Self::ThinkingDelta { .. } => "thinking_delta",
            Self::TextDelta { .. } => "text_delta",
            Self::ToolStart { .. } => "tool_start",
            Self::ToolResult { .. } => "tool_result",
            Self::TurnDone { .. } => "turn_done",
        }
    }
}

/// One key's entry: the record a reload replays, and the channel the
/// live watchers hang on. Both live behind one mutex, which is what
/// makes a history snapshot + subscription atomic.
struct KeyEntry {
    history: VecDeque<TurnEvent>,
    sender: broadcast::Sender<TurnEvent>,
}

/// The per-key record and broadcast of [`TurnEvent`]s.
pub struct EventHub {
    entries: Mutex<HashMap<ResourceKey, KeyEntry>>,
    capacity: usize,
    /// Where finished turns' records flush; `None` keeps the record
    /// memory-only (tests, and any assembly without a database).
    record_store: Option<Arc<TurnStore>>,
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
            entries: Mutex::new(HashMap::new()),
            capacity,
            record_store: None,
        }
    }

    /// A hub that flushes each key's record to `store` on every turn's
    /// end marker — the flush is what a restart's replay reads.
    pub fn with_record_store(store: Arc<TurnStore>) -> Self {
        Self {
            record_store: Some(store),
            ..Self::default()
        }
    }

    /// Subscribes to `key`'s turns, creating the channel on first use.
    /// Subscribe BEFORE triggering work: a turn that starts before the
    /// subscriber exists is simply not streamed to it. The chat
    /// surface's route uses [`Self::subscribe_with_history`] instead;
    /// this live-only shape is for callers that already know where the
    /// turn starts (the shim, the messages route).
    pub fn subscribe(&self, key: &ResourceKey) -> broadcast::Receiver<TurnEvent> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Self::entry(&mut entries, key, self.capacity)
            .sender
            .subscribe()
    }

    /// Subscribes to `key`'s turns and hands back the record cut at the
    /// same instant: everything emitted before this call is in
    /// `history`, everything after arrives on the receiver. The pair is
    /// cut under one lock, so no event is in both and none is in
    /// neither — a client folds `history` first and the stream after,
    /// and the transcript is whole.
    pub fn subscribe_with_history(
        &self,
        key: &ResourceKey,
    ) -> (Vec<TurnEvent>, broadcast::Receiver<TurnEvent>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        (
            entry.history.iter().cloned().collect(),
            entry.sender.subscribe(),
        )
    }

    /// Loads a key's record from the boot seed. Run before the surface
    /// serves: a subscriber arriving later replays exactly this.
    pub fn seed_history(&self, key: ResourceKey, events: Vec<TurnEvent>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, &key, self.capacity);
        entry.history = events.into_iter().collect();
        entry.history.truncate(RECORD_CAPACITY);
    }

    /// Publishes one event to `key`: the record takes it, and every
    /// live subscriber gets a copy. A key with nobody listening is not
    /// an error — turns run whether or not anyone watches, and the
    /// record keeps the turn replayable. The end marker also flushes
    /// the record to the store when one is attached: a crash between
    /// turns loses at most the turn that was still running.
    pub fn emit(&self, key: &ResourceKey, event: TurnEvent) {
        let snapshot = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = Self::entry(&mut entries, key, self.capacity);
            entry.history.push_back(event.clone());
            if entry.history.len() > RECORD_CAPACITY {
                entry.history.pop_front();
            }
            let _ = entry.sender.send(event);
            // The flush reads the record, not the just-pushed event: the
            // whole point is that the store ends up holding exactly what
            // the next boot seeds the hub with.
            matches!(entry.history.back(), Some(TurnEvent::TurnDone { .. }))
                .then(|| entry.history.iter().cloned().collect::<Vec<_>>())
        };
        if let (Some(store), Some(history)) = (self.record_store.as_ref(), snapshot) {
            if let Err(err) = store.save(key.as_str(), &history) {
                eprintln!("[claw] warning: turn-history flush for {key} failed: {err}");
            }
        }
    }

    /// The map entry for `key`, creating it (and its channel) on first
    /// use. Entries live for the process lifetime once created — the
    /// record is the point — and stay bounded by `RECORD_CAPACITY`.
    fn entry<'a>(
        entries: &'a mut HashMap<ResourceKey, KeyEntry>,
        key: &ResourceKey,
        capacity: usize,
    ) -> &'a mut KeyEntry {
        entries.entry(key.clone()).or_insert_with(|| KeyEntry {
            history: VecDeque::new(),
            sender: broadcast::channel(capacity).0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> ResourceKey {
        ResourceKey::from(format!("interactive:{name}"))
    }

    fn text(text: &str) -> TurnEvent {
        TurnEvent::TextDelta { text: text.into() }
    }

    fn done(reply: &str) -> TurnEvent {
        TurnEvent::TurnDone {
            reply: Some(reply.into()),
            outcome: "handled".into(),
        }
    }

    #[test]
    fn events_serialize_with_a_type_tag_and_snake_case() {
        assert_eq!(
            serde_json::to_string(&TurnEvent::TextDelta { text: "hi".into() }).unwrap(),
            r#"{"type":"text_delta","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&TurnEvent::UserMessage { text: "hi".into() }).unwrap(),
            r#"{"type":"user_message","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&TurnEvent::ToolResult {
                name: "bash".into(),
                is_error: true,
                summary: "exit 1".into(),
                code: Some(1),
                command: Some("false".into()),
            })
            .unwrap(),
            r#"{"type":"tool_result","name":"bash","is_error":true,"summary":"exit 1","code":1,"command":"false"}"#
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
    fn the_kind_is_the_wire_type_tag() {
        assert_eq!(
            TurnEvent::TextDelta { text: "x".into() }.kind(),
            "text_delta"
        );
        assert_eq!(
            TurnEvent::UserMessage { text: "x".into() }.kind(),
            "user_message"
        );
        assert_eq!(
            TurnEvent::TurnDone {
                reply: None,
                outcome: "handled".into()
            }
            .kind(),
            "turn_done"
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
        hub.emit(&key, done("done"));
        assert_eq!(
            receiver.try_recv().unwrap(),
            TurnEvent::ToolStart {
                name: "bash".into()
            }
        );
        assert_eq!(receiver.try_recv().unwrap(), done("done"));
    }

    #[test]
    fn a_history_subscription_replays_what_preceded_it_and_splits_exactly_once() {
        let hub = EventHub::default();
        let key = key("web");
        hub.emit(
            &key,
            TurnEvent::UserMessage {
                text: "hello".into(),
            },
        );
        hub.emit(&key, text("hi"));

        let (history, mut receiver) = hub.subscribe_with_history(&key);
        assert_eq!(
            history,
            vec![
                TurnEvent::UserMessage {
                    text: "hello".into()
                },
                text("hi"),
            ],
            "everything before the cut is in the record"
        );
        hub.emit(&key, done("hi back"));
        assert_eq!(
            receiver.try_recv().unwrap(),
            done("hi back"),
            "everything after the cut is on the stream"
        );

        // A second cut contains the whole run — the live events were
        // recorded too, and none of them twice.
        let (history, _) = hub.subscribe_with_history(&key);
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn emitting_to_nobody_listening_is_quiet_and_still_recorded() {
        let hub = EventHub::default();
        let key = key("alone");
        // No subscriber ever: quiet — and the record still accrues, so
        // a later viewer replays the turn nobody watched.
        hub.emit(&key, text("x"));
        assert!(
            hub.entries
                .lock()
                .unwrap()
                .get(&key)
                .expect("the entry exists")
                .sender
                .receiver_count()
                == 0
        );
        let (history, _) = hub.subscribe_with_history(&key);
        assert_eq!(history, vec![text("x")]);
    }

    #[test]
    fn keys_do_not_hear_each_other() {
        let hub = EventHub::default();
        let mut mine = hub.subscribe(&key("mine"));
        hub.subscribe(&key("theirs"));
        hub.emit(&key("theirs"), text("x"));
        assert!(mine.try_recv().is_err(), "no cross-talk");
    }

    #[test]
    fn a_lagging_subscriber_drops_the_oldest_and_is_told_so() {
        let hub = EventHub::with_capacity(2);
        let key = key("slow");
        let mut receiver = hub.subscribe(&key);
        for n in 1..=4 {
            hub.emit(&key, text(&n.to_string()));
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
            text("3"),
            "the newest capacity events survive"
        );
    }

    #[test]
    fn the_record_is_capped_at_the_newest_events() {
        let hub = EventHub::default();
        let key = key("long");
        for n in 0..RECORD_CAPACITY + 10 {
            hub.emit(&key, text(&n.to_string()));
        }
        let (history, _) = hub.subscribe_with_history(&key);
        assert_eq!(history.len(), RECORD_CAPACITY);
        assert_eq!(
            history.first(),
            Some(&text("10")),
            "the oldest fell off, not the newest"
        );
    }

    #[test]
    fn a_seeded_record_replays_and_keeps_taking_new_events() {
        let hub = EventHub::default();
        let key = key("booted");
        hub.seed_history(
            key.clone(),
            vec![TurnEvent::UserMessage {
                text: "from the last life".into(),
            }],
        );
        let (history, mut receiver) = hub.subscribe_with_history(&key);
        assert_eq!(history.len(), 1);
        hub.emit(&key, text("in this life"));
        assert_eq!(receiver.try_recv().unwrap(), text("in this life"));
    }

    #[test]
    fn the_end_marker_flushes_the_record_to_the_store() {
        let store = Arc::new(TurnStore::in_memory().expect("store opens"));
        let hub = EventHub::with_record_store(store.clone());
        let key = key("web");

        // Deltas alone do not flush: a crash mid-turn leaves the store
        // at the last turn boundary.
        hub.emit(
            &key,
            TurnEvent::UserMessage {
                text: "hello".into(),
            },
        );
        hub.emit(&key, text("hi"));
        assert_eq!(store.load(key.as_str()).expect("reads"), None);

        hub.emit(&key, done("hi back"));
        assert_eq!(
            store.load(key.as_str()).expect("reads"),
            Some(vec![
                TurnEvent::UserMessage {
                    text: "hello".into()
                },
                text("hi"),
                done("hi back"),
            ]),
            "the flushed record is exactly what a restart seeds"
        );
    }
}
