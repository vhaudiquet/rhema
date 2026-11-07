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

/// The furthest a bare history page widens while hunting for the turn
/// boundary it must start on, in events. A turn longer than this has
/// no boundary to find; the page then starts wherever the scan stops,
/// so the walk back through history always advances.
pub const HISTORY_SCAN_CAP: usize = 8192;

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

/// One recorded event with the seq the record carries for it: dense
/// per key from zero, assigned as the event is appended, never reused.
/// The seq is the wire frame's `id:`, so a client's cursor names an
/// exact place in the stream even across a reconnect.
pub type SeqEvent = (i64, TurnEvent);

/// What a replay hands back with its live receiver.
pub struct Replay {
    /// The events the caller asked for, oldest first, each with its seq.
    pub events: Vec<SeqEvent>,
    /// The caller's cursor names events this record no longer holds —
    /// a wiped table, or a memory window that has already dropped them.
    /// Its cache is invalid: it must drop what it has and rebuild from
    /// the tail, or the seqs it prints past this point would be lies.
    pub stale: bool,
}

/// One key's entry: the live channel, plus the in-memory record a
/// replay reads when no turn store is attached. Both live behind one
/// mutex, which is what makes a history snapshot + subscription atomic.
struct KeyEntry {
    history: VecDeque<SeqEvent>,
    /// The seq the next memory event takes; dense from zero.
    next_seq: i64,
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
        let (Replay { events, .. }, receiver) = self.replay_after(key, -1, 0);
        (
            events.into_iter().map(|(_, event)| event).collect(),
            receiver,
        )
    }

    /// The record past a cursor, cut at the same instant as the live
    /// receiver: every event with a seq greater than `after` is in
    /// `events`, everything emitted after the cut is on the receiver.
    /// `after` is `-1` for a client with nothing cached, which reads the
    /// whole record.
    ///
    /// A cursor the record can no longer serve (a wiped table, a pruned
    /// memory window) comes back `stale`, and the replay carries the
    /// newest `tail` events in place of the unreachable gap: the client
    /// drops what it has and rebuilds from those, instead of printing
    /// seqs that no longer name anything. The tail is read under the
    /// same lock as the receiver, so the reset has no hole either.
    pub fn replay_after(
        &self,
        key: &ResourceKey,
        after: i64,
        tail: usize,
    ) -> (Replay, broadcast::Receiver<TurnEvent>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        let store = self.record_store.as_deref();
        let (mut events, stale) = Self::record_after(store, entry, key, after);
        if stale {
            events = Self::record_tail(store, entry, key, tail);
        }
        (Replay { events, stale }, entry.sender.subscribe())
    }

    /// The record's newest `limit` events, cut with the live receiver —
    /// what a client with no cache opens with, so a session that ran
    /// for weeks paints its latest turn before anything older arrives.
    pub fn replay_tail(
        &self,
        key: &ResourceKey,
        limit: usize,
    ) -> (Replay, broadcast::Receiver<TurnEvent>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        let events = Self::record_tail(self.record_store.as_deref(), entry, key, limit);
        (
            Replay {
                events,
                stale: false,
            },
            entry.sender.subscribe(),
        )
    }

    /// One page of the record older than `before` (`None` reads from the
    /// newest), ending exactly at `before` and starting at a turn's
    /// first event — the page a client walking backward asks for, so a
    /// transcript never grows a fragment of a turn at its top. Reading
    /// back far enough to find a turn boundary widens the window; a
    /// record with no boundary in it (one turn longer than
    /// [`HISTORY_SCAN_CAP`] events) returns the widest window it read
    /// instead of nothing, so the walk always makes progress.
    pub fn replay_turn_before(
        &self,
        key: &ResourceKey,
        before: Option<i64>,
        limit: usize,
    ) -> Vec<SeqEvent> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        let store = self.record_store.as_deref();
        let mut want = limit.max(1);
        loop {
            let Some(window) = Self::record_before(store, entry, key, before, want) else {
                return Vec::new();
            };
            if window.is_empty() {
                return window;
            }
            if let Some(index) = window
                .iter()
                .position(|(_, event)| matches!(event, TurnEvent::UserMessage { .. }))
            {
                return window[index..].to_vec();
            }
            // The whole window sits inside one turn: read further back,
            // unless there is nothing further back to read.
            if window.len() < want || want >= HISTORY_SCAN_CAP {
                return window;
            }
            want = (want * 2).min(HISTORY_SCAN_CAP);
        }
    }

    /// Whether the record holds anything older than `seq` — the
    /// history walk's `more` flag, a lookup rather than a read.
    pub fn has_record_before(&self, key: &ResourceKey, seq: i64) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::entry(&mut entries, key, self.capacity);
        Self::record_before(self.record_store.as_deref(), entry, key, Some(seq), 1)
            .is_some_and(|window| !window.is_empty())
    }

    /// `after`'s events and whether the cursor outran the record.
    fn record_after(
        store: Option<&TurnStore>,
        entry: &KeyEntry,
        key: &ResourceKey,
        after: i64,
    ) -> (Vec<SeqEvent>, bool) {
        match store {
            Some(store) => match store.events_from(key.as_str(), after, usize::MAX) {
                Ok(rows) => {
                    // The table never drops rows, so a cursor past its
                    // newest seq can only mean the table was reset
                    // under a client that kept a cache of the old one.
                    let last = store.last_seq(key.as_str()).ok().flatten();
                    let stale = match last {
                        Some(last) => after > last,
                        None => after >= 0,
                    };
                    (rows, stale)
                }
                Err(err) => {
                    eprintln!("[claw] warning: replay for {key} failed: {err}");
                    // A record that cannot be read cannot be vouched
                    // for: the client rebuilds rather than trust a gap.
                    (Vec::new(), true)
                }
            },
            None => {
                let first = entry.history.front().map(|(seq, _)| *seq);
                let last = entry.history.back().map(|(seq, _)| *seq);
                let stale = match (first, last) {
                    // A pruned window it has passed, or a cursor ahead
                    // of the record it holds.
                    (Some(first), Some(last)) => {
                        after > last || (after >= 0 && after.saturating_add(1) < first)
                    }
                    _ => after >= 0,
                };
                let events = entry
                    .history
                    .iter()
                    .filter(|(seq, _)| *seq > after)
                    .cloned()
                    .collect();
                (events, stale)
            }
        }
    }

    /// The record's newest `limit` events.
    fn record_tail(
        store: Option<&TurnStore>,
        entry: &KeyEntry,
        key: &ResourceKey,
        limit: usize,
    ) -> Vec<SeqEvent> {
        match store {
            Some(store) => store
                .events_last(key.as_str(), limit)
                .unwrap_or_else(|err| {
                    eprintln!("[claw] warning: tail replay for {key} failed: {err}");
                    Vec::new()
                }),
            None => {
                let skip = entry.history.len().saturating_sub(limit);
                entry.history.iter().skip(skip).cloned().collect()
            }
        }
    }

    /// The newest `want` events below `before`, oldest first; `None`
    /// when the source could not be read.
    fn record_before(
        store: Option<&TurnStore>,
        entry: &KeyEntry,
        key: &ResourceKey,
        before: Option<i64>,
        want: usize,
    ) -> Option<Vec<SeqEvent>> {
        match store {
            Some(store) => store
                .events_before(key.as_str(), before.unwrap_or(i64::MAX), want)
                .map_err(|err| eprintln!("[claw] warning: history page for {key} failed: {err}"))
                .ok(),
            None => {
                let end = match before {
                    Some(before) => entry
                        .history
                        .iter()
                        .position(|(seq, _)| *seq >= before)
                        .unwrap_or(entry.history.len()),
                    None => entry.history.len(),
                };
                Some(
                    entry
                        .history
                        .iter()
                        .take(end)
                        .skip(end.saturating_sub(want))
                        .cloned()
                        .collect(),
                )
            }
        }
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
                entry.history.push_back((entry.next_seq, event.clone()));
                entry.next_seq += 1;
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
            next_seq: 0,
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

    /// The seq cursor: a client names the last event it holds and gets
    /// exactly what came after — no replay of what it has, no gap.
    #[test]
    fn a_seq_cursor_replays_exactly_the_events_past_it() {
        let hub = EventHub::default();
        let key = key("cached");
        for n in 0..4 {
            hub.emit(&key, text(&n.to_string()));
        }

        // Seqs are dense from zero: the fourth event is seq 3.
        let (replay, mut receiver) = hub.replay_after(&key, 2, 0);
        assert!(!replay.stale);
        assert_eq!(
            replay.events,
            vec![(3, text("3"))],
            "only the unseen part replays"
        );
        hub.emit(&key, done("hi back"));
        assert_eq!(receiver.try_recv().unwrap(), done("hi back"));

        // A cursor at the record's newest replays nothing and is not
        // stale: the client is simply up to date.
        let (replay, _) = hub.replay_after(&key, 4, 0);
        assert!(replay.events.is_empty());
        assert!(!replay.stale);

        // Nothing cached behaves as the plain replay.
        let (replay, _) = hub.replay_after(&key, -1, 0);
        assert_eq!(replay.events.len(), 5);
        assert_eq!(replay.events.first().map(|(seq, _)| *seq), Some(0));
    }

    /// A cursor the record cannot serve is reported, and the tail comes
    /// back in place of the gap: a memory window that dropped the events,
    /// or a record that was wiped under the client.
    #[test]
    fn a_cursor_past_the_record_is_stale_and_gets_the_tail() {
        let hub = EventHub::with_capacity(16);
        let key = key("wiped");
        for n in 0..3 {
            hub.emit(&key, text(&n.to_string()));
        }
        // A cursor from a record this one never had (seq 40): stale, and
        // the tail is what replaces the gap.
        let (replay, _) = hub.replay_after(&key, 40, 2);
        assert!(replay.stale, "a cursor ahead of the record is stale");
        assert_eq!(replay.events, vec![(1, text("1")), (2, text("2"))]);
    }

    /// A memory window that pruned events the client still names: the
    /// gap is unreachable, so it is stale too.
    #[test]
    fn a_cursor_below_a_pruned_window_is_stale() {
        let hub = EventHub::default();
        let key = key("pruned");
        for n in 0..RECORD_CAPACITY + 5 {
            hub.emit(&key, text(&n.to_string()));
        }
        // The window now starts at seq 5; a client parked at seq 0 can
        // never be served events 1..5 again.
        let (replay, _) = hub.replay_after(&key, 0, 1);
        assert!(replay.stale);
        assert_eq!(replay.events.len(), 1, "the tail replaces the gap");
    }

    /// The tail is the newest events, with their seqs intact.
    #[test]
    fn the_tail_is_the_newest_events() {
        let hub = EventHub::default();
        let key = key("tail");
        for n in 0..6 {
            hub.emit(&key, text(&n.to_string()));
        }
        let (replay, _) = hub.replay_tail(&key, 2);
        assert!(!replay.stale);
        assert_eq!(replay.events, vec![(4, text("4")), (5, text("5"))]);
    }

    /// Backward paging: each page ends where the last began and starts
    /// on a turn boundary, so a transcript stacks pages without gaps,
    /// overlaps, or a fragment at its top.
    #[test]
    fn a_backward_page_starting_on_a_turn_boundary_stacks_exactly() {
        let hub = EventHub::default();
        let key = key("paged");
        // Three turns of three events each: question, reply, done.
        for turn in 0..3 {
            hub.emit(
                &key,
                TurnEvent::UserMessage {
                    text: format!("q{turn}"),
                    images: Vec::new(),
                },
            );
            hub.emit(&key, text(&format!("a{turn}")));
            hub.emit(&key, done(&format!("a{turn}")));
        }
        // A window of 4 from the end (seqs 5..8) opens mid-turn; the
        // page cuts back to the question at seq 6.
        let page = hub.replay_turn_before(&key, None, 4);
        assert_eq!(page.len(), 3, "one whole turn");
        assert_eq!(page.first().map(|(seq, _)| *seq), Some(6));
        assert!(hub.has_record_before(&key, 6));

        // The next page ends exactly at that first seq.
        let page = hub.replay_turn_before(&key, Some(6), 4);
        assert_eq!(page.first().map(|(seq, _)| *seq), Some(3));
        assert_eq!(page.len(), 3);

        // The first page reaches the record's start and says so.
        let page = hub.replay_turn_before(&key, Some(3), 4);
        assert_eq!(page.first().map(|(seq, _)| *seq), Some(0));
        assert!(!hub.has_record_before(&key, 0), "nothing below the first");
    }
}
