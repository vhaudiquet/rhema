//! The normalized event vocabulary shared by every calendar backend.
//!
//! Constraints the normalization pins (each adapter module documents how
//! it meets them):
//! - An event is one occurrence, never a series. Recurring masters are
//!   expanded (by the Google server with `singleEvents=true`, by the
//!   `ics` adapter client-side), so a consumer never sees a rule, only
//!   starts it can compare against its clock.
//! - [`CalendarEvent::id`] is the dedup id a dispatcher keys on:
//!   `{provider}:{uid}@{unix start}`. Two polls of the same occurrence
//!   produce the same id; the id changes only when the occurrence
//!   itself is a different one. What a consumer does about the
//!   repetition (dispatch dedup, reminder slots) is the consumer's
//!   seam, not this crate's.
//! - Times are instants, not wall-clock strings. A wire time with no
//!   offset reads as UTC, and an all-day date starts at its midnight
//!   UTC with [`CalendarEvent::all_day`] set, so every `start`
//!   compares against any clock without a timezone database on board.
//! - [`CalendarEvent::end`] is exclusive, as every wire format spells
//!   it, and `None` when the row names no end.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::serde::rfc3339;

/// One occurrence on a calendar: a meeting, a birthday, a block of
/// time. Everything a reminder needs to say, lifted from whichever
/// dialect produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Stable dedup id, `{provider}:{uid}@{unix start}` (e.g.
    /// `ics:c1f3a9@1792471200`,
    /// `google:_70ojsm0k6nojajk6@1792471200`): one occurrence, one id,
    /// across polls. A recurring series expands to one event per
    /// occurrence, each with its own.
    pub id: String,
    /// The title (`SUMMARY` / `summary`); empty when the row has none.
    pub title: String,
    /// When the occurrence starts.
    #[serde(with = "rfc3339")]
    pub start: OffsetDateTime,
    /// When it ends, exclusive; `None` when the row names no end.
    #[serde(with = "rfc3339::option")]
    pub end: Option<OffsetDateTime>,
    /// Whether this is an all-day occurrence: the wire carried a date
    /// with no time, and [`CalendarEvent::start`] is that date's
    /// midnight UTC.
    pub all_day: bool,
    /// Where it happens, when the row says.
    pub location: Option<String>,
    /// The row's notes, when it has any.
    pub description: Option<String>,
    /// Who is invited, bare addresses (`mailto:` stripped, Google's
    /// `email` fields).
    pub attendees: Vec<String>,
    /// The organizer's address, when the row names one.
    pub organizer: Option<String>,
    /// Human-facing URL of the event (`URL` / `htmlLink`).
    pub url: Option<String>,
    /// The untouched wire row: the adapter's parsed property map for
    /// ICS, the JSON object for Google. Parameters the vocabulary does
    /// not lift (`TZID`, for one) ride here for a consumer that can do
    /// more with them.
    pub raw: serde_json::Value,
}

impl CalendarEvent {
    /// The event's duration: `end - start` when the row names an end,
    /// zero otherwise. All-day rows without an `DTEND` are a zero-width
    /// midnight: the wire says the day, the duration is not this
    /// crate's guess to widen.
    pub fn duration(&self) -> time::Duration {
        self.end
            .map(|end| end - self.start)
            .unwrap_or(time::Duration::ZERO)
    }
}

/// The stretch of time a poll asks about, as starts: an event belongs
/// to a window when its [`CalendarEvent::start`] falls inside
/// `[from, to)`. Windows are absolute (instants, not cursors), which
/// is why a calendar poll needs no cursor: re-asking the same window
/// returns the same rows under the same ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The earliest start included.
    pub from: OffsetDateTime,
    /// The latest start excluded.
    pub to: OffsetDateTime,
}

impl Window {
    /// A window from `from` (inclusive) to `to` (exclusive).
    pub fn new(from: OffsetDateTime, to: OffsetDateTime) -> Self {
        Self { from, to }
    }

    /// Whether `start` falls inside the window.
    pub fn starts_within(&self, start: OffsetDateTime) -> bool {
        start >= self.from && start < self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
            .expect("rfc3339 parses")
    }

    #[test]
    fn a_window_bounds_by_start_inclusively_below_exclusively() {
        let window = Window::new(at("2026-10-20T00:00:00Z"), at("2026-10-21T00:00:00Z"));
        assert!(window.starts_within(at("2026-10-20T00:00:00Z")));
        assert!(window.starts_within(at("2026-10-20T23:59:59Z")));
        assert!(!window.starts_within(at("2026-10-19T23:59:59Z")));
        assert!(!window.starts_within(at("2026-10-21T00:00:00Z")));
    }

    #[test]
    fn an_event_round_trips_through_serde() {
        let event = CalendarEvent {
            id: "ics:abc@1".into(),
            title: "standup".into(),
            start: at("2026-10-20T09:00:00Z"),
            end: Some(at("2026-10-20T09:15:00Z")),
            all_day: false,
            location: None,
            description: None,
            attendees: vec!["a@example.net".into()],
            organizer: Some("b@example.net".into()),
            url: None,
            raw: serde_json::json!({ "SUMMARY": "standup" }),
        };
        let round: CalendarEvent =
            serde_json::from_value(serde_json::to_value(&event).expect("serializes"))
                .expect("deserializes");
        assert_eq!(round, event);
        assert_eq!(round.duration(), time::Duration::minutes(15));
    }

    #[test]
    fn an_endless_event_has_no_duration() {
        let event = CalendarEvent {
            id: "ics:abc@1".into(),
            title: String::new(),
            start: at("2026-10-20T09:00:00Z"),
            end: None,
            all_day: true,
            location: None,
            description: None,
            attendees: Vec::new(),
            organizer: None,
            url: None,
            raw: serde_json::json!({}),
        };
        assert_eq!(event.duration(), time::Duration::ZERO);
    }
}
