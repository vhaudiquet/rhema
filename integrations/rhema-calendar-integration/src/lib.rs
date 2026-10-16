//! `rhema-calendar-integration` — the calendar integration: one
//! normalized occurrence vocabulary and one client contract for the
//! calendar feeds a resident agent can remind about, with the adapters
//! that read them.
//!
//! An integration, not an agent: no driver, no loop, and no reminder
//! policy lives here. A consumer composes the crate with its own agent
//! tier — it picks a [`crate::Window`], polls a [`crate::Calendar`]
//! for the occurrences starting inside it, and decides what a start
//! time means (the claw daemon today: one reminder slot per lead time,
//! dispatched into the resident agent). Every [`crate::CalendarEvent`]
//! carries an id that stays the same across polls of the same
//! occurrence, so a consumer re-polling on its own cadence never
//! re-fires a reminder it already fed; the repetition is the
//! consumer's dispatcher's dedup, not this crate's problem.
//!
//! Recurrence is expanded before a poll answers, so a consumer never
//! sees a rule, only starts it can compare against its clock:
//! - The Google Calendar adapter asks the server to expand
//!   (`singleEvents=true`).
//! - The ICS adapter expands client-side, covering the rule shapes
//!   feeds actually emit (daily, weekly with `BYDAY`, monthly,
//!   yearly; `INTERVAL`, `COUNT`, `UNTIL`, `EXDATE`) and applies
//!   `RECURRENCE-ID` overrides. Anything outside those shapes
//!   surfaces once, at its `DTSTART`, never silently dropped. A
//!   native CalDAV adapter would reuse the same expansion over
//!   `REPORT` payloads; the feed-URL adapter covers Nextcloud,
//!   Radicale, and Google's secret iCal address without the XML.
//!
//! Times are instants, not wall-clock strings. Every wire format (RFC
//! 3339, iCalendar's compact `YYYYMMDDTHHMMSS[Z]`, `VALUE=DATE`
//! all-days) normalizes to a `time::OffsetDateTime`; a floating
//! (offset-less) time reads as UTC, and a `TZID` parameter rides the
//! event's `raw` for a consumer that can resolve zones.
//!
//! ```
//! use rhema_calendar_integration::{Calendar, IcsCalendar, Window};
//! use time::format_description::well_known::Rfc3339;
//!
//! // A feed is one URL (webcal counts as https); the poll is one GET.
//! let feed = IcsCalendar::new("https://example.net/nowhere/calendar.ics");
//! let from = time::OffsetDateTime::parse("2026-10-20T00:00:00Z", &Rfc3339)
//!     .expect("parses");
//! let window = Window::new(from, from + time::Duration::hours(24));
//! // `feed.events(&window).await` answers with the occurrences whose
//! // start falls inside the window, oldest first; this doctest only
//! // assembles them.
//! let _ = (&feed, &window);
//! ```
//!
//! Layout:
//! - [`event`] — the normalized vocabulary: `CalendarEvent`, `Window`
//! - [`error`] — the crate's single calendar error type
//! - [`calendar`] — the `Calendar` client contract
//! - [`ics`] — the ICS/webcal adapter (one GET, expanded client-side)
//! - [`google`] — the Google Calendar API adapter (expanded server-side)

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod calendar;
pub mod error;
pub mod event;
pub mod google;
pub mod ics;

// Re-exported so implementors of the async seam can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use calendar::Calendar;
pub use error::CalendarError;
pub use event::{CalendarEvent, Window};
pub use google::GoogleCalendar;
pub use ics::IcsCalendar;
