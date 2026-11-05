//! The reminder source: deterministic detection of due reminders, the
//! task scheduler's shape over a calendar feed.
//!
//! The [`CalendarSource`] is a [`Source`] named `"calendars"` on the
//! resident dispatcher: its `detect` pass polls every configured
//! calendar for the occurrences starting within its horizon and
//! ingests one event per due reminder slot, a (occurrence, lead time)
//! pair whose moment has come. The dedup id carries all three facts
//! (`{calendar}:{occurrence id}:{lead}`), and the next lead's slot is
//! a different id firing later. A slot feeds once: the dispatcher
//! dedups a fed id only while the slot sits in its pending inbox, so
//! the source keeps its own ledger of fed slots in the calendar store
//! and skips anything already there. The store-backed source's ledger
//! survives a restart; a static-list source keeps its ledger in
//! memory, so its slots dedup for the process's lifetime. A slot
//! whose start has passed is never fed: a reminder never fires after
//! the fact. All-day occurrences
//! never fire: a lead in minutes against a midnight start is noise,
//! and a date-shaped nudge is what the task tools are for.
//!
//! This is the default reminder policy, shipped beside the feeds it
//! polls: one slot per lead time, `calendar:<name>` keys, all-day
//! occurrences never fired. A consumer with different semantics brings
//! its own source — the vocabulary's stable occurrence ids keep any
//! implementation dedup-safe — but every reminder-shaped one starts
//! here. The model is never reached; the turn happens because the
//! consumer's dispatcher admitted the `calendar:<name>` key. One
//! session per calendar persists across its reminders, so the resident
//! remembers what it said about the last one. A dead feed is logged
//! and skipped, never fatal: an outage must not eat the other
//! calendars' reminders on the same pass. Cadence belongs to the
//! consumer's loop.

use std::sync::Arc;

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_rsd::{Clock, Dispatcher, Event, KeyFn, ResourceKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::FedSlot;
use crate::{
    CaldavCalendar, Calendar, CalendarError, CalendarEvent, GoogleCalendar, IcsCalendar, Window,
};

/// The source name the calendar reminders ingest under; registered on
/// the consumer's dispatcher, so allowlists and ingest doors can name
/// it.
pub const CALENDARS_SOURCE: &str = "calendars";

/// The key prefix the reminder sessions live under: `calendar:<name>`.
pub const KEY_PREFIX: &str = "calendar:";

/// The env variable a calendar's token falls back to.
pub const TOKEN_ENV: &str = "CALENDAR_TOKEN";

/// The env variable a CalDAV password falls back to.
pub const CALDAV_PASSWORD_ENV: &str = "CALDAV_PASSWORD";

/// One calendar's reminder key: `calendar:<name>` — the session the
/// reminder turns run in, persistent across the calendar's reminders.
pub fn calendar_key(name: &str) -> String {
    format!("{KEY_PREFIX}{name}")
}

/// The key function the source registers: the payload's `key`
/// verbatim — one key, one owned session — with the infallible
/// fallback, so a keyless payload can neither clobber a session nor
/// wedge the dispatcher; it lands in the unroutable slot.
fn reminder_key_fn() -> Arc<KeyFn> {
    Arc::new(|event: &Event| {
        event
            .payload
            .get("key")
            .and_then(Value::as_str)
            .map(ResourceKey::from)
            .unwrap_or_else(|| ResourceKey::from(format!("{KEY_PREFIX}unroutable#{}", event.id)))
    })
}

/// One configured calendar, resolved to a client the source polls.
#[derive(Clone)]
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
fn client_for(calendar: &CalendarWatch) -> Result<Arc<dyn Calendar>, CalendarError> {
    match calendar.kind {
        CalendarKind::Google => {
            let id = calendar
                .calendar_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| CalendarError::Config("google needs calendar_id".to_owned()))?;
            if let Some(key) = calendar.resolved_api_key() {
                return Ok(Arc::new(GoogleCalendar::with_api_key(key, id)));
            }
            let token = calendar
                .resolved_token()
                .ok_or_else(|| CalendarError::Config("google needs api_key or token".to_owned()))?;
            Ok(Arc::new(GoogleCalendar::new(token, id)))
        }
        CalendarKind::Ics => {
            let url = calendar
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| CalendarError::Config("ics needs url".to_owned()))?;
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
                .ok_or_else(|| CalendarError::Config("caldav needs url".to_owned()))?;
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
                    CalendarError::Config(
                        "caldav needs a password: set password or CALDAV_PASSWORD".to_owned(),
                    )
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
    /// Where this pass's feeds come from: a static list (tests, and
    /// callers that hold the specs) or the calendar table, read fresh
    /// on every detect pass so a runtime add is live next tick.
    feeds: FeedSource,
    /// The fed-slot ledger, the durable half of the reminder dedup:
    /// the dispatcher forgets a fed id once its turn drains the
    /// inbox, and without this the next tick would re-ingest the
    /// identical id and re-dispatch the same reminder for the whole
    /// lead window. The store-backed source shares the calendar
    /// table's database; a static-list source keeps its ledger in
    /// memory.
    ledger: Arc<crate::store::CalendarStore>,
}

enum FeedSource {
    Static(Vec<Feed>),
    Store(Arc<crate::store::CalendarStore>),
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
    ) -> Result<Self, CalendarError> {
        let mut feeds = Vec::with_capacity(calendars.len());
        for calendar in calendars {
            feeds.push(Feed {
                name: calendar.name.trim().to_owned(),
                client: client_for(calendar)?,
                leads: calendar.leads(),
                horizon_hours: calendar.horizon(),
            });
        }
        // A static-list source carries no database file, so its
        // ledger lives in memory: slots dedup for the process's
        // lifetime, and a restart re-feeds them.
        let ledger = Arc::new(
            crate::store::CalendarStore::in_memory()
                .map_err(|err| CalendarError::Store(format!("fed-slot ledger: {err}")))?,
        );
        dispatcher.register_source(CALENDARS_SOURCE, reminder_key_fn());
        Ok(Self {
            dispatcher: dispatcher.clone(),
            clock,
            feeds: FeedSource::Static(feeds),
            ledger,
        })
    }

    /// Builds from the calendar table instead of a static list: the
    /// source reads it fresh every pass, so a calendar added over the
    /// HTTP route (or a chat tool) is live without a restart, and a
    /// removed one stops polling on the next tick. The same store
    /// backs the fed-slot ledger, so dedup outlives a restart. Config
    /// validation ran when the row landed; a calendar that still fails
    /// to resolve here names itself and costs its tick, not the
    /// daemon.
    pub fn from_store(
        dispatcher: &Arc<Dispatcher>,
        clock: Arc<dyn Clock>,
        store: Arc<crate::store::CalendarStore>,
    ) -> Self {
        dispatcher.register_source(CALENDARS_SOURCE, reminder_key_fn());
        Self {
            dispatcher: dispatcher.clone(),
            clock,
            feeds: FeedSource::Store(store.clone()),
            ledger: store,
        }
    }

    /// The feeds this source polls, resolved for one detect pass.
    fn feeds_for_pass(&self) -> Result<Vec<Feed>, String> {
        match &self.feeds {
            FeedSource::Static(feeds) => Ok(feeds.to_vec()),
            FeedSource::Store(store) => store_feeds(store),
        }
    }

    /// A source over ready-made feeds, with the ledger passed in (a
    /// fresh in-memory store per test, or a reopened one for the
    /// restart tests). Tests use this; production builds from the
    /// config with [`CalendarSource::new`] or [`CalendarSource::from_store`].
    #[cfg(test)]
    pub(crate) fn from_feeds(
        dispatcher: &Arc<Dispatcher>,
        clock: Arc<dyn Clock>,
        feeds: Vec<Feed>,
        ledger: Arc<crate::store::CalendarStore>,
    ) -> Self {
        dispatcher.register_source(CALENDARS_SOURCE, reminder_key_fn());
        Self {
            dispatcher: dispatcher.clone(),
            clock,
            feeds: FeedSource::Static(feeds),
            ledger,
        }
    }
}

/// Resolves the table's calendars into this pass's feeds. A feed's
/// client is rebuilt per pass — the table is small, and correctness
/// (a runtime add is live) beats a pool the row could outlive.
fn store_feeds(store: &crate::store::CalendarStore) -> Result<Vec<Feed>, String> {
    let calendars = store
        .list()
        .map_err(|err| format!("calendar table: {err}"))?;
    let mut feeds = Vec::with_capacity(calendars.len());
    for calendar in &calendars {
        feeds.push(Feed {
            name: calendar.name.trim().to_owned(),
            // The Source seam reports through `SourceError`, whose text
            // carries the refusal; the typed config error stringifies
            // at that boundary.
            client: client_for(calendar).map_err(|err| err.to_string())?,
            leads: calendar.leads(),
            horizon_hours: calendar.horizon(),
        });
    }
    Ok(feeds)
}

#[async_trait]
impl Source for CalendarSource {
    fn name(&self) -> &str {
        CALENDARS_SOURCE
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        reminder_key_fn()
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let now_ms = self.clock.now_millis() as i64;
        let now = OffsetDateTime::from_unix_timestamp_nanos(now_ms as i128 * 1_000_000)
            .map_err(|err| SourceError::Backend(format!("clock: {err}")))?;
        // Housekeeping rides the pass it already needs: the prune
        // never removes a slot that could still fire, and failing it
        // costs table space, not correctness.
        if let Err(err) = self.ledger.prune_fed_slots(now.unix_timestamp()) {
            eprintln!("[calendar] warning: fed-slot prune failed: {err}");
        }
        let mut fed = 0;
        let feeds = self.feeds_for_pass().map_err(SourceError::Backend)?;
        for feed in &feeds {
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
                    eprintln!("[calendar] warning: {} feed failed: {err}", feed.name);
                    continue;
                }
            };
            for (event, lead) in due_slots(&events, &feed.leads, now) {
                // A slot the ledger already fed is skipped, not
                // re-ingested: the dispatcher's dedup only holds while
                // the slot sits pending, so the ledger is what keeps
                // the tick from re-firing it. A ledger read or write
                // that fails fails loudly: a silently lost row is a
                // re-fired reminder.
                let already_fed = self
                    .ledger
                    .slot_is_fed(&feed.name, &event.id, lead)
                    .map_err(|err| SourceError::Backend(format!("fed-slot ledger: {err}")))?;
                if already_fed {
                    continue;
                }
                self.dispatcher.ingest(reminder_event(
                    &feed.name,
                    event,
                    lead,
                    now,
                    now_ms as u64,
                ))?;
                let slot = FedSlot {
                    calendar: feed.name.clone(),
                    occurrence: event.id.clone(),
                    lead,
                    starts_at: event.start.unix_timestamp(),
                };
                self.ledger
                    .mark_slot_fed(&slot, now.unix_timestamp())
                    .map_err(|err| SourceError::Backend(format!("fed-slot ledger: {err}")))?;
                fed += 1;
            }
        }
        Ok(fed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CalendarError;
    use crate::store::CalendarStore;
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
        async fn events(&self, _window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
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

    /// A fresh in-memory fed-slot ledger for one test's source.
    fn ledger() -> Arc<CalendarStore> {
        Arc::new(CalendarStore::in_memory().expect("ledger opens"))
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
            ledger(),
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

        // The next pass finds the slot in the ledger and skips it:
        // nothing re-fed, however many ticks remain in the lead
        // window, and nothing re-dispatched once the inbox drains.
        assert_eq!(source.detect().await.expect("detect runs"), 0);
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

        // At 15 minutes out the second slot fires under its own id;
        // the first slot's ledger row keeps its replay out of the
        // pass. One event per lead, never more.
        clock.advance(45 * 60 * 1000);
        assert_eq!(source.detect().await.expect("detect runs"), 1);
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
        // slot is due. Both are fed on that first pass; the second
        // finds both rows in the ledger and feeds nothing.
        let source = source(&dispatcher, clock.clone(), vec![event("late", "late", 15)]);
        assert_eq!(source.detect().await.expect("detect runs"), 2);
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher), 2);
    }

    #[tokio::test]
    async fn a_reopened_store_still_suppresses_fed_slots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("calendars.db");
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let feeds = || {
            vec![Feed {
                name: "home".to_owned(),
                client: Arc::new(Fixed(vec![event("standup", "standup", 90)])),
                leads: vec![60],
                horizon_hours: 24,
            }]
        };

        // Sixty minutes out the slot is due and feeds, its row
        // landing in the file-backed ledger.
        clock.advance(30 * 60 * 1000);
        {
            let store = Arc::new(CalendarStore::open(&path).expect("store opens"));
            let dispatcher = dispatcher();
            let source = CalendarSource::from_feeds(&dispatcher, clock.clone(), feeds(), store);
            assert_eq!(source.detect().await.expect("detect runs"), 1);
        }

        // A restart: the source, its dispatcher, and its pending
        // inbox are all gone, and the slot is still due. The ledger
        // is what survives, and it is what keeps the reminder from
        // firing a second time.
        clock.advance(30 * 60 * 1000);
        let store = Arc::new(CalendarStore::open(&path).expect("store reopens"));
        let dispatcher = dispatcher();
        let source = CalendarSource::from_feeds(&dispatcher, clock.clone(), feeds(), store);
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&dispatcher), 0);
    }

    #[tokio::test]
    async fn a_fed_slot_is_not_refed_when_the_inbox_has_drained() {
        // The dispatcher dedups by (source, id) against pending items
        // only, so a fresh dispatcher is a drained inbox: whatever it
        // holds, the old behavior re-fed. The ledger, not the
        // dispatcher, is the reason this stays quiet.
        let clock = Arc::new(FixedClock::new(NOW_MS as u64));
        let ledger = ledger();
        let feeds = vec![Feed {
            name: "home".to_owned(),
            client: Arc::new(Fixed(vec![event("standup", "standup", 90)])),
            leads: vec![60, 15],
            horizon_hours: 24,
        }];

        // Sixty minutes out, the 60 slot feeds once.
        clock.advance(30 * 60 * 1000);
        let first = dispatcher();
        let source =
            CalendarSource::from_feeds(&first, clock.clone(), feeds.clone(), ledger.clone());
        assert_eq!(source.detect().await.expect("detect runs"), 1);

        // A second source over the same store and the same feed, with
        // an empty dispatcher: the 60 slot is in the ledger, the 15
        // slot is not yet due, so nothing feeds.
        clock.advance(30 * 60 * 1000);
        let second = dispatcher();
        let source = CalendarSource::from_feeds(&second, clock.clone(), feeds, ledger);
        assert_eq!(source.detect().await.expect("detect runs"), 0);
        assert_eq!(inbox_depth(&second), 0);
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
            async fn events(&self, _window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
                Err(CalendarError::Network("down".to_owned()))
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
            ledger(),
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

/// The one reminder lead, in minutes, a calendar carries when
/// [`CalendarWatch::lead_minutes`] is unset: warn shortly before, and
/// let a busier lead list be configured where wanted.
pub const DEFAULT_LEAD_MINUTES: u64 = 15;

/// How far ahead a calendar polls, in hours, when
/// [`CalendarWatch::horizon_hours`] is unset. Must exceed the longest
/// lead: a reminder cannot fire before the event is visible.
pub const DEFAULT_HORIZON_HOURS: u32 = 24;

/// Which calendar feed a calendar watch polls. The wire spelling is
/// the source name the dispatcher registers and the reminder prompt
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarKind {
    /// An iCalendar feed over HTTP (`webcal://` counts): Google's
    /// secret iCal address, a Nextcloud or Radicale share, Fastmail.
    Ics,
    /// A CalDAV collection, polled with a `calendar-query` REPORT:
    /// Nextcloud, Radicale, Fastmail, a plain DAV server.
    Caldav,
    /// The Google Calendar API, against an OAuth access token or an
    /// API key for public calendars.
    Google,
}

impl CalendarKind {
    /// The spelling the config, the reminder source, and the prompts
    /// use.
    pub fn name(self) -> &'static str {
        match self {
            CalendarKind::Ics => "ics",
            CalendarKind::Caldav => "caldav",
            CalendarKind::Google => "google",
        }
    }

    /// Parses a calendar kind as the config spells it. `None` for
    /// anything else.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "ics" => Some(CalendarKind::Ics),
            "caldav" => Some(CalendarKind::Caldav),
            "google" => Some(CalendarKind::Google),
            _ => None,
        }
    }
}

impl CalendarWatch {
    /// The rules a calendar must satisfy to poll — the checks the
    /// config's boot validation used to run, now at the one door every
    /// writer shares: the table's `add`.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be blank".to_owned());
        }
        match self.kind {
            CalendarKind::Ics => {
                let url = nonempty(self.url.as_deref()).ok_or_else(|| {
                    "ics is a feed URL: set url to the calendar's address".to_owned()
                })?;
                if !(url.starts_with("https://")
                    || url.starts_with("http://")
                    || url.starts_with("webcal://"))
                {
                    return Err(format!(
                        "url must be https://, http://, or webcal://, got {url:?}"
                    ));
                }
            }
            CalendarKind::Caldav => {
                let url = nonempty(self.url.as_deref()).ok_or_else(|| {
                    "caldav is a collection: set url to the calendar's DAV address".to_owned()
                })?;
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(format!("url must be https:// or http://, got {url:?}"));
                }
                let has_user = nonempty(self.username.as_deref()).is_some();
                if has_user && self.resolved_password().is_none() {
                    return Err("no password: set password or CALDAV_PASSWORD".to_owned());
                }
            }
            CalendarKind::Google => {
                if nonempty(self.calendar_id.as_deref()).is_none() {
                    return Err(
                        "google needs a calendar: set calendar_id (\"primary\", an address, a hex id)"
                            .to_owned(),
                    );
                }
                if self.resolved_api_key().is_none() && self.resolved_token().is_none() {
                    return Err(
                        "no credential: set api_key (public calendars) or token, or CALENDAR_TOKEN"
                            .to_owned(),
                    );
                }
            }
        }
        // The horizon is what the poll can see; a lead at or past it
        // fires on an event that was never visible.
        let horizon_minutes = u64::from(self.horizon()) * 60;
        let longest = self.leads().into_iter().max().unwrap_or_default();
        if longest >= horizon_minutes {
            return Err(format!(
                "lead {longest}m is not under the {horizon_minutes}m horizon: a reminder cannot fire before its event is visible"
            ));
        }
        Ok(())
    }
}

/// One watched calendar. Unknown keys are rejected, as on a watch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct CalendarWatch {
    /// The calendar's name: unique across the list; the reminder key
    /// is `calendar:<name>` and prompts name the calendar by it.
    pub name: String,
    /// Which feed kind this polls.
    pub kind: CalendarKind,
    /// The feed URL (`https://` or `webcal://`). Required for ics.
    pub url: Option<String>,
    /// The Google calendar id (`primary`, an address, a hex id).
    /// Required for google.
    pub calendar_id: Option<String>,
    /// A bearer token (an OAuth access token for Google, auth for a
    /// feed or collection that demands it). Falls back to
    /// `CALENDAR_TOKEN`.
    pub token: Option<String>,
    /// The CalDAV user, for Basic auth. Set only with a password (or
    /// `CALDAV_PASSWORD`); a calendar with a username authenticates
    /// Basic, and `token` is the fallback only when there is none.
    pub username: Option<String>,
    /// The CalDAV password or app-specific password. Falls back to
    /// `CALDAV_PASSWORD`.
    pub password: Option<String>,
    /// A Google API key for a public calendar; wins over `token`.
    pub api_key: Option<String>,
    /// Reminder lead times, minutes; each occurrence warns once per
    /// lead. `[DEFAULT_LEAD_MINUTES]` when unset.
    pub lead_minutes: Option<Vec<u64>>,
    /// How far ahead the poll looks, hours. `[DEFAULT_HORIZON_HOURS]`
    /// when unset; must exceed the longest lead.
    pub horizon_hours: Option<u32>,
}

impl CalendarWatch {
    /// The reminder leads as configured, blank and zero entries
    /// dropped; [`DEFAULT_LEAD_MINUTES`] alone when nothing usable is
    /// configured.
    pub fn leads(&self) -> Vec<u64> {
        match self.lead_minutes.as_ref() {
            Some(leads) => {
                let usable: Vec<u64> = leads.iter().copied().filter(|lead| *lead > 0).collect();
                if usable.is_empty() {
                    vec![DEFAULT_LEAD_MINUTES]
                } else {
                    usable
                }
            }
            None => vec![DEFAULT_LEAD_MINUTES],
        }
    }

    /// The poll horizon as configured, [`DEFAULT_HORIZON_HOURS`] when
    /// unset. Zero reads as the default: a horizon of nothing polls
    /// nothing, and the config validation refuses it when it would
    /// matter.
    pub fn horizon(&self) -> u32 {
        self.horizon_hours
            .filter(|hours| *hours > 0)
            .unwrap_or(DEFAULT_HORIZON_HOURS)
    }

    /// A trimmed `token`/`api_key` value; blank counts as unset.
    fn trimmed(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    /// The API key as configured, if any.
    pub fn resolved_api_key(&self) -> Option<String> {
        Self::trimmed(self.api_key.as_deref())
    }

    /// The CalDAV password to authenticate with: the config value,
    /// then `CALDAV_PASSWORD`. Parameterized over the env lookup in
    /// the pure [`CalendarWatch::resolve_password_with`].
    pub fn resolved_password(&self) -> Option<String> {
        Self::resolve_password_with(|name| std::env::var(name), self.password.as_deref())
    }

    /// Pure password resolution: the config value first, then
    /// `CALDAV_PASSWORD`. Parameterized over the env lookup so tests
    /// can hold the environment still.
    pub fn resolve_password_with(
        get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
        stored: Option<&str>,
    ) -> Option<String> {
        Self::trimmed(stored).or_else(|| {
            get_env("CALDAV_PASSWORD")
                .ok()
                .and_then(|token| Self::trimmed(Some(&token)))
        })
    }

    /// The token to poll with: the config value, then `CALENDAR_TOKEN`;
    /// the first non-blank wins. Parameterized over the env lookup in
    /// the pure [`CalendarWatch::resolve_token_with`].
    pub fn resolved_token(&self) -> Option<String> {
        Self::resolve_token_with(|name| std::env::var(name), self.token.as_deref())
    }

    /// Pure token resolution: the config value first, then
    /// `CALENDAR_TOKEN`. Parameterized over the env lookup so tests can
    /// hold the environment still.
    pub fn resolve_token_with(
        get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
        stored: Option<&str>,
    ) -> Option<String> {
        Self::trimmed(stored).or_else(|| {
            get_env("CALENDAR_TOKEN")
                .ok()
                .and_then(|token| Self::trimmed(Some(&token)))
        })
    }
}
