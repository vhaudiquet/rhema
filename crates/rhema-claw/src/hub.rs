//! The event hub: per-key broadcast of live turn events, and the
//! per-key record a late subscriber replays.
//!
//! HTTP subscribers stream one key's turns through it: a chat UI holds
//! one `GET /v1/sessions/{key}/events` connection open and watches every
//! turn of that session go by — whoever triggered the turn. The hub is
//! a [`tokio::sync::broadcast`] channel per key behind a mutex'd map.
//! `emit` records the event and broadcasts in one lock; a subscriber
//! that asks for history gets the record and a live receiver cut under
//! that same lock — every event lands in exactly one of the two, so a
//! replay has no gap and no overlap with the live stream, ever.
//!
//! Where the record lives depends on the assembly. With a turn store
//! attached (the daemon), every event is appended there as it streams
//! and a replay reads the store back: the transcript is unbounded and
//! survives restarts, so a session that runs for weeks replays whole —
//! the appends serialize under the hub's lock, which is what keeps the
//! table's order the stream's order. Without one (tests, store-less
//! assemblies) the record is a bounded in-memory window
//! ([`RECORD_CAPACITY`] events per key, the oldest falling off), good
//! enough for a process that lives as long as its transcript needs to.
//!
//! Live delivery itself stays best-effort: a subscriber that falls more
//! than [`EventHub::capacity`] events behind drops the oldest ones
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

/// How many events one key's in-memory record holds when no turn store
/// is attached. Wide enough that a reload rebuilds whole recent turns —
/// thinking traces and tool rows are deltas, and they add up.
pub const RECORD_CAPACITY: usize = 8192;

/// One image a chat message carried, as the transcript records it: the
/// attachment store's id and the media type the bytes carry. The
/// transcript streams ids, never megabytes — a client fetches the
/// bytes through `GET /v1/attachments/{id}`, the route the agent's own
/// attachments already serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserImage {
    /// The attachment store's id — the fetch route's address.
    pub id: String,
    /// The full MIME type (`image/png`).
    pub media_type: String,
}

/// One event of a turn, as the wire sees it. Serialized with an internal
/// `type` tag, snake-cased: `{"type":"text_delta","text":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// The turn's input: the text (a batch folds into one) and the
    /// images the message batch carried, recorded by the handler when
    /// the dispatch starts. It exists for the replay — a chat surface
    /// shows its own messages as it sends them, and skips the live
    /// echo.
    UserMessage {
        /// The input text, verbatim.
        text: String,
        /// The batch's images, in arrival order, as the attachment
        /// store holds them. `default` because rows recorded before
        /// images rode the transcript carry no field.
        #[serde(default)]
        images: Vec<UserImage>,
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
        /// `handled` or `failed` — or `steered`, when a chat message
        /// cut the turn short and a next turn runs instead of it.
        outcome: String,
    },
    /// The agent asked the human something only they can answer: a
    /// credential to store, permission to act. A chat surface renders
    /// the ask as a card and answers through the consent routes.
    ConsentRequest {
        /// The request's id — the answer addresses it.
        id: String,
        /// What kind of answer: `secret` or `phone`.
        kind: String,
        /// The one-line ask.
        summary: String,
        /// The ask's context, the model's own reason.
        detail: String,
        /// When the request was made, epoch millis — a card that has
        /// outlived the answer window greys out against it.
        created_at: u64,
    },
    /// A consent request settled. The tool's answer rides the turn like
    /// any tool result; this is how the card learns it can stop waiting.
    ConsentResolved {
        /// The request's id.
        id: String,
        /// Whether it was granted.
        granted: bool,
    },
    /// The agent attached a file from its workspace: the metadata a
    /// card needs, the bytes behind `GET /v1/attachments/{id}`.
    Attachment {
        /// The attachment's id.
        id: String,
        /// The file's name, as the tool call named it.
        name: String,
        /// What the name implies: `image`, `code`, or `file`.
        media: String,
        /// The byte size.
        size: u64,
        /// One line about the file, when the tool call carried one.
        caption: Option<String>,
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
            Self::ConsentRequest { .. } => "consent_request",
            Self::ConsentResolved { .. } => "consent_resolved",
            Self::Attachment { .. } => "attachment",
        }
    }
}

/// One key's entry: the live channel, plus the in-memory record a
/// replay reads when no turn store is attached. Both live behind one
/// mutex, which is what makes a history snapshot + subscription atomic.
struct KeyEntry {
    history: VecDeque<TurnEvent>,
    sender: broadcast::Sender<TurnEvent>,
}

/// The per-key record and broadcast of [`TurnEvent`]s.
pub struct EventHub {
    entries: Mutex<HashMap<ResourceKey, KeyEntry>>,
    capacity: usize,
    /// Where events append as they stream; `None` keeps the record
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

    /// A hub that appends every event to `store` as it streams — the
    /// replay a restart serves reads back exactly what streamed, with no
    /// window and no end-marker boundary.
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
        Self::subscribe_with_history_from(self, key, 0)
    }

    /// [`Self::subscribe_with_history`] with a cursor: `skip` names how
    /// many of the record's oldest events the caller already holds (the
    /// count a client tracks for its local transcript cache), and the
    /// replay drops them, so a reconnecting client that kept its
    /// transcript receives only what is new. A `skip` past the record's
    /// end clamps to it — the client cannot skip into events that do not
    /// exist, and the live feed after the cut is unaffected either way.
    pub fn subscribe_with_history_from(
        &self,
        key: &ResourceKey,
        skip: usize,
    ) -> (Vec<TurnEvent>, broadcast::Receiver<TurnEvent>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        let mut history = match self.record_store.as_ref() {
            Some(store) => store.events(key.as_str()).unwrap_or_else(|err| {
                eprintln!("[claw] warning: replay for {key} failed: {err}");
                Vec::new()
            }),
            None => entry.history.iter().cloned().collect(),
        };
        history.drain(..skip.min(history.len()));
        (history, entry.sender.subscribe())
    }

    /// Publishes one event to `key`: the record takes it, and every
    /// live subscriber gets a copy. A key with nobody listening is not
    /// an error — turns run whether or not anyone watches, and the
    /// record keeps the turn replayable. With a store attached the
    /// append happens under the same lock as the broadcast, so the
    /// table's order is the stream's order no matter how events race in
    /// across keys.
    pub fn emit(&self, key: &ResourceKey, event: TurnEvent) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        match self.record_store.as_ref() {
            Some(store) => {
                if let Err(err) = store.append(key.as_str(), std::slice::from_ref(&event)) {
                    eprintln!("[claw] warning: turn-history append for {key} failed: {err}");
                }
            }
            None => {
                entry.history.push_back(event.clone());
                if entry.history.len() > RECORD_CAPACITY {
                    entry.history.pop_front();
                }
            }
        }
        let _ = entry.sender.send(event);
    }

    /// The map entry for `key`, creating it (and its channel) on first
    /// use. Entries live for the process lifetime once created — the
    /// live channel is the point; with a store attached the record
    /// outlives the process, without one it stays bounded by
    /// `RECORD_CAPACITY`.
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
            serde_json::to_string(&TurnEvent::UserMessage {
                text: "hi".into(),
                images: Vec::new(),
            })
            .unwrap(),
            r#"{"type":"user_message","text":"hi","images":[]}"#
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

    /// Rows recorded before images rode the transcript carry no
    /// `images` field; they replay as plain questions.
    #[test]
    fn a_user_message_row_from_before_images_loads_without_them() {
        let event: TurnEvent =
            serde_json::from_str(r#"{"type":"user_message","text":"hello"}"#).unwrap();
        assert_eq!(
            event,
            TurnEvent::UserMessage {
                text: "hello".into(),
                images: Vec::new()
            }
        );
    }

    #[test]
    fn the_kind_is_the_wire_type_tag() {
        assert_eq!(
            TurnEvent::TextDelta { text: "x".into() }.kind(),
            "text_delta"
        );
        assert_eq!(
            TurnEvent::UserMessage {
                text: "x".into(),
                images: Vec::new()
            }
            .kind(),
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
                images: Vec::new(),
            },
        );
        hub.emit(&key, text("hi"));

        let (history, mut receiver) = hub.subscribe_with_history(&key);
        assert_eq!(
            history,
            vec![
                TurnEvent::UserMessage {
                    text: "hello".into(),
                    images: Vec::new()
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
    fn the_memory_record_is_capped_at_the_newest_events() {
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

    /// The restart story: a store-backed hub's record outlives the
    /// process, whole — a fresh hub over the same store replays
    /// everything, and past the in-memory cap, too.
    #[test]
    fn a_stored_record_replays_whole_across_hubs_and_past_any_cap() {
        let store = Arc::new(TurnStore::in_memory().expect("store opens"));
        let hub = EventHub::with_record_store(store.clone());
        let key = key("long");
        for n in 0..RECORD_CAPACITY + 10 {
            hub.emit(&key, text(&n.to_string()));
        }
        hub.emit(&key, done("hi back"));

        let restarted = EventHub::with_record_store(store);
        let (history, mut receiver) = restarted.subscribe_with_history(&key);
        assert_eq!(
            history.len(),
            RECORD_CAPACITY + 11,
            "nothing fell off: the table, not a window, is the record"
        );
        assert_eq!(history.first(), Some(&text("0")));
        assert_eq!(history.last(), Some(&done("hi back")));
        restarted.emit(&key, text("in this life"));
        assert_eq!(
            receiver.try_recv().unwrap(),
            text("in this life"),
            "the live cut is exact: replayed events never re-deliver"
        );
    }

    #[test]
    fn a_memory_record_replays_and_keeps_taking_new_events() {
        let hub = EventHub::default();
        let key = key("booted");
        hub.emit(
            &key,
            TurnEvent::UserMessage {
                text: "earlier".into(),
                images: Vec::new(),
            },
        );
        let (history, mut receiver) = hub.subscribe_with_history(&key);
        assert_eq!(history.len(), 1);
        hub.emit(&key, text("in this life"));
        assert_eq!(receiver.try_recv().unwrap(), text("in this life"));
    }

    #[test]
    fn a_cursor_skips_exactly_the_events_the_caller_already_holds() {
        let hub = EventHub::default();
        let key = key("cached");
        for n in 0..4 {
            hub.emit(&key, text(&n.to_string()));
        }

        // A client that cached the first three receives only the fourth;
        // the live cut is untouched by the skip.
        let (history, mut receiver) = hub.subscribe_with_history_from(&key, 3);
        assert_eq!(history, vec![text("3")], "only the unseen part replays");
        hub.emit(&key, done("hi back"));
        assert_eq!(receiver.try_recv().unwrap(), done("hi back"));

        // A cursor past the record's end clamps to it: nothing replays,
        // and nothing the record still holds is skipped twice.
        let (history, _) = hub.subscribe_with_history_from(&key, 100);
        assert!(history.is_empty());

        // Zero behaves as the plain replay.
        let (history, _) = hub.subscribe_with_history_from(&key, 0);
        assert_eq!(history.len(), 5);
    }
}
