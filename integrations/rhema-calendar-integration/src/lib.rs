//! `rhema-calendar-integration` — the calendar integration: one
//! normalized occurrence vocabulary and one client contract for the
//! calendar feeds a resident agent can remind about, with the adapters
//! that read them.
//!
//! An integration, not an agent: no driver, no model, and no loop
//! lives here. A consumer composes the crate with its own agent tier —
//! it picks a [`crate::Window`], polls a [`crate::Calendar`] for the
//! occurrences starting inside it, and decides what a start time
//! means. The default answer ships in [`crate::reminder`]: one
//! reminder slot per lead time, keyed `calendar:<name>`, fed to the
//! consumer's dispatcher as a `Source` — every reminder-shaped
//! consumer starts there. A consumer with different semantics brings
//! its own; every [`crate::CalendarEvent`] carries an id that stays
//! the same across polls of the same occurrence, so any
//! implementation re-polling on its own cadence never re-fires a
//! reminder it already fed. The repetition is the consumer's
//! dispatcher's dedup, not this crate's problem.
//!
//! Recurrence is expanded before a poll answers, so a consumer never
//! sees a rule, only starts it can compare against its clock:
//! - The Google Calendar adapter asks the server to expand
//!   (`singleEvents=true`).
//! - The ICS and CalDAV adapters expand client-side, covering the
//!   rule shapes servers actually emit (daily, weekly with `BYDAY`,
//!   monthly, yearly; `INTERVAL`, `COUNT`, `UNTIL`, `EXDATE`) and
//!   applying `RECURRENCE-ID` overrides. Anything outside those
//!   shapes surfaces once, at its `DTSTART`, never silently dropped.
//!   CalDAV shares the exact expansion: the server returns masters
//!   plus overrides in range, and the adapter reads them out of the
//!   multistatus. Where a server exposes both, the feed URL and the
//!   DAV collection are different identities: their occurrence ids
//!   carry different provider prefixes.
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
//! - [`caldav`] — the CalDAV adapter (one REPORT, the same expansion)
//! - [`google`] — the Google Calendar API adapter (expanded server-side)
//! - [`reminder`] — the default reminder source: due slots per lead
//!   time, `calendar:<name>` keys, one `Source` over the configured feeds

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod caldav;
pub mod calendar;
pub mod error;
pub mod event;
pub mod google;
pub mod ics;
pub mod reminder;
pub mod store;
pub use store::CalendarStore;

// Re-exported so implementors of the async seam can apply #[async_trait]
// without adding the crate themselves.
pub use async_trait::async_trait;
pub use caldav::CaldavCalendar;
pub use calendar::Calendar;
pub use error::CalendarError;
pub use event::{CalendarEvent, Window};
pub use google::GoogleCalendar;
pub use ics::IcsCalendar;
pub use reminder::{CalendarKind, CalendarSource, CalendarWatch, calendar_key};
