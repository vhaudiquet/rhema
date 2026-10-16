//! The calendar source: deterministic detection of due reminders, the
//! task scheduler's shape over a calendar feed.
//!
//! The [`CalendarSource`] is a [`Source`] named `"calendars"` on the
//! resident dispatcher: its `detect` pass polls every configured
//! calendar for the occurrences starting within its horizon and
//! ingests one event per due reminder slot, a (occurrence, lead time)
//! pair whose moment has come. The dedup id carries all three facts
//! (`{calendar}:{occurrence id}:{lead}`), so a slot feeds once and its
//! repeats dedup away in the dispatcher, and the next lead's slot is a
//! different id firing later. A slot whose start has passed is never
//! fed: a reminder never fires after the fact. All-day occurrences
//! never fire: a lead in minutes against a midnight start is noise,
//! and a date-shaped nudge is what the task tools are for.
//!
//! The model is never reached here; the turn happens because the
//! dispatcher admitted the `calendar:<name>` key, exactly as for a
//! task firing, and the resident handler's delivery rule auto-notifies
//! the final text to the user's devices. One session per calendar
//! persists across its reminders, so the claw remembers what it said
//! about the last one. A dead feed is logged and skipped, never fatal:
//! an outage must not eat the other calendars' reminders on the same
//! pass. Cadence belongs to the daemon's loop.

use std::sync::Arc;

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_calendar_integration::{
    CaldavCalendar, Calendar, CalendarEvent, GoogleCalendar, IcsCalendar, Window,
};
use rhema_rsd::{Clock, Dispatcher, Event, KeyFn};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::config::{CalendarKind, CalendarWatch};
use crate::interactive::passthrough_key_fn;

/// The source name the calendar reminders ingest under; registered on
/// the resident dispatcher, so allowlists and `/v1/ingest` can name it.
pub const CALENDARS_SOURCE: &str = "calendars";

/// The env variable a calendar's token falls back to.
pub const TOKEN_ENV: &str = "CALENDAR_TOKEN";

/// The env variable a CalDAV password falls back to.
pub const CALDAV_PASSWORD_ENV: &str = "CALDAV_PASSWORD";

/// One calendar's reminder key: `calendar:<name>` — the session the
/// reminder turns run in, persistent across the calendar's reminders.
pub fn calendar_key(name: &str) -> String {
    format!("calendar:{name}")
}

/// One configured calendar, resolved to a client the source polls.
pub(crate) struct Feed {
    name: String,
    client: Arc<dyn Calendar>,
    leads: Vec<u64>,
    horizon_hours: u32,
}

/// The client one calendar watch polls through. Credentials resolve
/// before construction: the api key wins for Google, else the token
/// (config first, then [`TOKEN_ENV`]), and a feed takes its token or
/// polls anonymous, which is how most published feed URLs work.
fn client_for(calendar: &CalendarWatch) -> Result<Arc<dyn Calendar>, String> {
    match calendar.kind {
        CalendarKind::Google => {
            let id = calendar
                .calendar_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "google needs calendar_id".to_owned())?;
            if let Some(key) = calendar.resolved_api_key() {
                return Ok(Arc::new(GoogleCalendar::with_api_key(key, id)));
            }
            let token = calendar
                .resolved_token()
                .ok_or_else(|| "google needs api_key or token".to_owned())?;
            Ok(Arc::new(GoogleCalendar::new(token, id)))
        }
        CalendarKind::Ics => {
            let url = calendar
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| "ics needs url".to_owned())?;
            let mut feed = IcsCalendar::new(url);
            if let Some(token) = calendar.resolved_token() {
                feed = feed.with_token(token);
            }
            Ok(Arc::new(feed))
        }
        CalendarKind::Caldav => {
            let url = calendar
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| "caldav needs url".to_owned())?;
            let mut dav = CaldavCalendar::new(url);
            // A username means Basic auth, the self-hosted norm, and
            // the password must have resolved at validation; a token
            // without one is the bearer spelling; neither polls
            // anonymous, which is what open servers take.
            if let Some(user) = calendar
                .username
                .as_deref()
                .map(str::trim)
                .filter(|user| !user.is_empty())
            {
                let password = calendar.resolved_password().ok_or_else(|| {
                    "caldav needs a password: set password or CALDAV_PASSWORD".to_owned()
                })?;
                dav = dav.with_basic(user, password);
            } else if let Some(token) = calendar.resolved_token() {
                dav = dav.with_token(token);
            }
            Ok(Arc::new(dav))
        }
    }
}

/// The reminder slots of one poll's events: (event, lead) pairs that
/// are due, in feed order, leads in configured order. Due means the
/// lead's warning moment has arrived and the event has not started;
/// all-day events and started events contribute nothing.
pub(crate) fn due_slots<'a>(
    events: &'a [CalendarEvent],
    leads: &[u64],
    now: OffsetDateTime,
) -> Vec<(&'a CalendarEvent, u64)> {
    let now_secs = now.unix_timestamp();
    let mut slots = Vec::new();
    for event in events {
        if event.all_day {
            continue;
        }
        let start = event.start.unix_timestamp();
        if start <= now_secs {
            continue;
        }
        for lead in leads {
            if start - i64::try_from(*lead).unwrap_or(i64::MAX) * 60 <= now_secs {
                slots.push((event, *lead));
            }
        }
    }
    slots
}

/// The event one due slot becomes: keyed to the calendar's session,
/// its dedup id naming the calendar, the occurrence, and the lead, so
/// a refired slot dedups and the next lead does not.
pub(crate) fn reminder_event(
    calendar: &str,
    event: &CalendarEvent,
    lead: u64,
    now: OffsetDateTime,
    received_at: u64,
) -> Event {
    // The start is strictly future at this point, so the count is at
    // least one.
    let minutes = ((event.start - now).whole_seconds() + 59) / 60;
    let title = match event.title.trim() {
        "" => "(untitled)",
        trimmed => trimmed,
    };
    let start = event.start.format(&Rfc3339).unwrap_or_default();
    let mut lines = vec![
        format!(
            "Calendar reminder from the daemon (source `{CALENDARS_SOURCE}`), not a user message."
        ),
        format!("Calendar {calendar:?}: {title:?} starts in {minutes} minutes at {start}."),
    ];
    if let Some(location) = nonempty(event.location.as_deref()) {
        lines.push(format!("Where: {location}"));
    }
    if let Some(notes) = nonempty(event.description.as_deref()) {
        lines.push(format!("Notes: {notes}"));
    }
    if let Some(url) = nonempty(event.url.as_deref()) {
        lines.push(format!("Link: {url}"));
    }
    if !event.attendees.is_empty() {
        lines.push(format!("Attendees: {}", event.attendees.join(", ")));
    }
    if let Some(organizer) = nonempty(event.organizer.as_deref()) {
        lines.push(format!("Organizer: {organizer}"));
    }
    lines.push(
        "Reply with one short line telling the user; this session's reply \
         auto-notifies their devices."
            .to_owned(),
    );

    Event {
        id: format!("{calendar}:{}:{lead}", event.id),
        source: CALENDARS_SOURCE.to_owned(),
        payload: json!({ "key": calendar_key(calendar), "prompt": lines.join("\n") }),
        received_at,
    }
}

/// A trimmed string, or `None` when it is all whitespace.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The detectable stream of calendar reminders: every configured
/// calendar, polled on the daemon's tick, feeding the resident
/// dispatcher.
pub struct CalendarSource {
    dispatcher: Arc<Dispatcher>,
    clock: Arc<dyn Clock>,
    feeds: Vec<Feed>,
}

impl CalendarSource {
    /// Registers the `calendars` source's key function on `dispatcher`
    /// and resolves `calendars` into clients. Config validation ran at
    /// load; a calendar that still fails here names itself in the
    /// error, and the daemon refuses to start.
    pub fn new(
        dispatcher: &Arc<Dispatcher>,
        clock: Arc<dyn Clock>,
        calendars: &[CalendarWatch],
    ) -> Result<Self, String> {
        let mut feeds = Vec::with_capacity(calendars.len());
        for calendar in calendars {
            feeds.push(Feed {
                name: calendar.name.trim().to_owned(),
                client: client_for(calendar)?,
                leads: calendar.leads(),
                horizon_hours: calendar.horizon(),
            });
        }
        dispatcher.register_source(CALENDARS_SOURCE, passthrough_key_fn());
        Ok(Self {
            dispatcher: dispatcher.clone(),
            clock,
            feeds,
        })
    }

    /// A source over ready-made feeds. Tests use this; production
    /// builds from the config with [`CalendarSource::new`].
    #[cfg(test)]
    pub(crate) fn from_feeds(
        dispatcher: &Arc<Dispatcher>,
        clock: Arc<dyn Clock>,
        feeds: Vec<Feed>,
    ) -> Self {
        dispatcher.register_source(CALENDARS_SOURCE, passthrough_key_fn());
        Self {
            dispatcher: dispatcher.clone(),
            clock,
            feeds,
        }
    }
}

#[async_trait]
impl Source for CalendarSource {
    fn name(&self) -> &str {
        CALENDARS_SOURCE
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        passthrough_key_fn()
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let now_ms = self.clock.now_millis() as i64;
        let now = OffsetDateTime::from_unix_timestamp_nanos(now_ms as i128 * 1_000_000)
            .map_err(|err| SourceError::Backend(format!("clock: {err}")))?;
        let mut fed = 0;
        for feed in &self.feeds {
            let window = Window::new(
                now,
                now + time::Duration::hours(i64::from(feed.horizon_hours)),
            );
            let events = match feed.client.events(&window).await {
                Ok(events) => events,
                // A dead feed is this tick's loss, not the daemon's:
                // the next pass polls it again, and the other
                // calendars' reminders never wait behind it.
                Err(err) => {
                    eprintln!("[claw] warning: calendar {} failed: {err}", feed.name);
                    continue;
                }
            };
            for (event, lead) in due_slots(&events, &feed.leads, now) {
                self.dispatcher.ingest(reminder_event(
                    &feed.name,
                    event,
                    lead,
                    now,
                    now_ms as u64,
                ))?;
                fed += 1;
            }
        }
        Ok(fed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_rsd::{DispatcherConfig, FixedClock, ResourceKey, SystemClock, VolatileStore};

    /// 2026-10-20T00:00:00Z, in milliseconds.
    const NOW_MS: i64 = 1_792_444_800_000;

    fn dispatcher() -> Arc<Dispatcher> {
        Arc::new(Dispatcher::new(
            DispatcherConfig {
                debounce_window_millis: 0,
                ..DispatcherConfig::default()
            },
            Arc::new(SystemClock),
            Arc::new(VolatileStore::new()),
        ))
    }

    /// A feed that answers with fixed events, whatever the window.
    struct Fixed(Vec<CalendarEvent>);

    #[async_trait]
    impl Calendar for Fixed {
        async fn events(
            &self,
            _window: &Window,
        ) -> Result<Vec<CalendarEvent>, rhema_calendar_integration::CalendarError> {
            Ok(self.0.clone())
        }
    }

    fn event(id: &str, title: &str, minutes_from_now: i64) -> CalendarEvent {
        let start = OffsetDateTime::from_unix_timestamp(NOW_MS / 1000 + minutes_from_now * 60)
            .expect("timestamp");
        CalendarEvent {
            id: format!("ics:{id}@{}", start.unix_timestamp()),
            title: title.to_owned(),
            start,
            end: Some(start + time::Duration::minutes(30)),
            all_day: false,
            location: None,
            description: None,
            attendees: Vec::new(),
            organizer: None,
            url: None,
            raw: serde_json::json!({}),
        }
    }

    fn source(
        dispatcher: &Arc<Dispatcher>,
        clock: Arc<dyn Clock>,
        events: Vec<CalendarEvent>,
    ) -> CalendarSource {
        CalendarSource::from_feeds(
            dispatcher,
            clock,
            vec![Feed {
                name: "home".to_owned(),
                client: Arc::new(Fixed(events)),
                leads: vec![60, 15],
                horizon_hours: 24,
            }],
        )
    }

    fn inbox_depth(dispatcher: &Dispatcher) -> usize {
        dispatcher
            .status(&ResourceKey::from(calendar_key("home")))
            .expect("status reads")
            .inbox_depth
    }

    #[tokio::test]
    async fn a_due_slot_fires_once_and_stays_deduped() {
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let dispatcher = dispatcher();
        let source = source(
            &dispatcher,
            clock.clone(),
            vec![event("standup", "standup", 90)],
        );

        // Ninety minutes out, neither lead is due yet.
        assert_eq!(source.detect().await.expect("detect runs"), 0);

        // Sixty minutes out the first slot is due: one event on the
        // calendar's key.
        clock.advance(30 * 60 * 1000);
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        assert_eq!(inbox_depth(&dispatcher), 1);

        // The next pass replays the same dedup id: fed, deduped away.
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        assert_eq!(inbox_depth(&dispatcher), 1);
    }

    #[tokio::test]
    async fn every_lead_fires_once_at_its_own_moment() {
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let dispatcher = dispatcher();
        // Starts in 90 minutes: the 60 lead is not due yet.
        let source = source(&dispatcher, clock.clone(), vec![event("demo", "demo", 90)]);
        assert_eq!(source.detect().await.expect("detect runs"), 0);

        // At 60 minutes out the first slot fires.
        clock.advance(30 * 60 * 1000);
        assert_eq!(source.detect().await.expect("detect runs"), 1);

        // At 15 minutes out the second slot fires under its own id.
        // The count includes the first slot's replay, which dedups:
        // the inbox holds one event per lead, never more.
        clock.advance(45 * 60 * 1000);
        assert_eq!(source.detect().await.expect("detect runs"), 2);
        assert_eq!(inbox_depth(&dispatcher), 2);

        // Past the start: nothing more, ever.
        clock.advance(20 * 60 * 1000);
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher), 2);
    }

    #[tokio::test]
    async fn a_slot_first_seen_late_fires_late_once() {
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let dispatcher = dispatcher();
        // The daemon only meets this event 15 minutes before it
        // starts: the 60 slot's moment came and went unseen, the 15
        // slot is due. Both are fed on that first pass, then dedup.
        let source = source(&dispatcher, clock.clone(), vec![event("late", "late", 15)]);
        assert_eq!(source.detect().await.expect("detect runs"), 2);
        assert_eq!(source.detect().await.expect("detect runs"), 2);
        assert_eq!(inbox_depth(&dispatcher), 2);
    }

    #[tokio::test]
    async fn a_started_or_all_day_event_never_fires() {
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let dispatcher = dispatcher();
        let mut all_day = event("offsite", "offsite", 10);
        all_day.all_day = true;
        let started = event("past", "already going", -5);
        let source = source(&dispatcher, clock.clone(), vec![all_day, started]);
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher), 0);
    }

    #[tokio::test]
    async fn a_dead_feed_is_skipped_not_fatal() {
        struct Dead;

        #[async_trait]
        impl Calendar for Dead {
            async fn events(
                &self,
                _window: &Window,
            ) -> Result<Vec<CalendarEvent>, rhema_calendar_integration::CalendarError> {
                Err(rhema_calendar_integration::CalendarError::Network(
                    "down".to_owned(),
                ))
            }
        }

        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let dispatcher = dispatcher();
        let source = CalendarSource::from_feeds(
            &dispatcher,
            clock.clone(),
            vec![
                Feed {
                    name: "dead".to_owned(),
                    client: Arc::new(Dead),
                    leads: vec![15],
                    horizon_hours: 24,
                },
                Feed {
                    name: "alive".to_owned(),
                    client: Arc::new(Fixed(vec![event("alive-event", "alive", 10)])),
                    leads: vec![15],
                    horizon_hours: 24,
                },
            ],
        );
        assert_eq!(source.detect().await.expect("detect runs"), 1);
        assert_eq!(
            dispatcher
                .status(&ResourceKey::from(calendar_key("alive")))
                .expect("status reads")
                .inbox_depth,
            1
        );
    }

    #[test]
    fn the_prompt_names_everything_a_reply_needs() {
        let now = OffsetDateTime::from_unix_timestamp(NOW_MS / 1000).expect("timestamp");
        let mut event = event("dentist", "dentist", 15);
        event.location = Some("rue de la Paix".to_owned());
        event.description = Some("cleaning".to_owned());
        event.attendees = vec!["dr@example.net".to_owned()];
        event.organizer = Some("dr@example.net".to_owned());
        event.url = Some("https://meet.example.net/x".to_owned());

        let slot = reminder_event("home", &event, 15, now, NOW_MS as u64);
        assert_eq!(slot.source, CALENDARS_SOURCE);
        assert_eq!(
            slot.id,
            format!("home:{}:15", event.id),
            "the dedup id names calendar, occurrence, and lead"
        );
        assert_eq!(slot.payload["key"], json!(calendar_key("home")));
        let prompt = slot.payload["prompt"].as_str().expect("prompt");
        assert!(prompt.contains("\"dentist\""), "got: {prompt}");
        assert!(prompt.contains("in 15 minutes"), "got: {prompt}");
        assert!(prompt.contains("home"), "got: {prompt}");
        assert!(prompt.contains("rue de la Paix"), "got: {prompt}");
        assert!(prompt.contains("cleaning"), "got: {prompt}");
        assert!(prompt.contains("dr@example.net"), "got: {prompt}");
        assert!(
            prompt.contains("https://meet.example.net/x"),
            "got: {prompt}"
        );
        assert!(prompt.contains("auto-notifies"), "got: {prompt}");
    }

    #[test]
    fn an_untitled_event_still_reads_as_a_reminder() {
        let now = OffsetDateTime::from_unix_timestamp(NOW_MS / 1000).expect("timestamp");
        let event = event("bare", "  ", 3);
        let slot = reminder_event("home", &event, 15, now, NOW_MS as u64);
        let prompt = slot.payload["prompt"].as_str().expect("prompt");
        assert!(prompt.contains("(untitled)"), "got: {prompt}");
        assert!(prompt.contains("in 3 minutes"), "got: {prompt}");
    }
}
