//! The calendar client contract: one trait, one call.
//!
//! [`Calendar`] is the whole surface a consumer needs: the occurrences
//! starting inside a window. Implementations are cheap to clone and
//! share (the reqwest client inside is reference-counted) and
//! object-safe so a backend can be swapped without generics.
//!
//! Polls are at-least-once and window-anchored. Nothing here tracks a
//! cursor, because a calendar is addressable by time, not by history:
//! re-asking the same window returns the same rows under the same
//! ids, and the consumer's dispatcher dedups the repetition. There is
//! deliberately no write half: reminders read a calendar; what a
//! consumer does about one is an agent's business, not an adapter's.

use async_trait::async_trait;

use crate::error::CalendarError;
use crate::event::{CalendarEvent, Window};

/// A calendar backend: the client for one feed dialect, bound to one
/// calendar (the URL for ICS, the calendar id for Google).
#[async_trait]
pub trait Calendar: Send + Sync {
    /// The occurrences whose start falls in `window`, oldest first,
    /// ties broken by id. Recurring series arrive expanded: one
    /// [`CalendarEvent`] per occurrence, overrides applied, cancelled
    /// occurrences gone.
    async fn events(&self, window: &Window) -> Result<Vec<CalendarEvent>, CalendarError>;
}

/// Folds collected events into an adapter's answer: keeps the rows
/// starting in `window`, drops duplicate ids (first wins), and sorts
/// by `(start, id)`. Adapters call this before returning, so a
/// consumer reads one deterministic order regardless of which feed
/// produced the rows.
pub(crate) fn finish(mut events: Vec<CalendarEvent>, window: &Window) -> Vec<CalendarEvent> {
    events.retain(|event| window.starts_within(event.start));
    events.sort_by(|a, b| (&a.start, &a.id).cmp(&(&b.start, &b.id)));
    events.dedup_by(|a, b| a.id == b.id);
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: &str, start: &str) -> CalendarEvent {
        CalendarEvent {
            id: id.to_owned(),
            title: "x".to_owned(),
            start: time::OffsetDateTime::parse(
                start,
                &time::format_description::well_known::Rfc3339,
            )
            .expect("parses"),
            end: None,
            all_day: false,
            location: None,
            description: None,
            attendees: Vec::new(),
            organizer: None,
            url: None,
            raw: json!({}),
        }
    }

    #[test]
    fn finish_clips_sorts_and_dedups() {
        let window = Window::new(at("2026-10-20T00:00:00Z"), at("2026-10-21T00:00:00Z"));
        let events = finish(
            vec![
                event("b", "2026-10-20T10:00:00Z"),
                event("z", "2026-10-19T10:00:00Z"), // before: dropped
                event("a", "2026-10-20T10:00:00Z"),
                event("b", "2026-10-20T10:00:00Z"), // duplicate id: dropped
                event("c", "2026-10-21T00:00:00Z"), // at `to`: dropped
            ],
            &window,
        );
        let ids: Vec<&str> = events.iter().map(|event| event.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    fn at(raw: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
            .expect("parses")
    }
}
