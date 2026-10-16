//! The Google Calendar adapter: the Calendar API v3 events list.
//!
//! Auth is one credential, chosen by what the deployment has: an OAuth
//! access token in `Authorization: Bearer` (the header only; the token
//! never lands in a URL or a log line), or an API key for public
//! calendars, which Google only accepts as the `key` query parameter.
//!
//! Endpoint choices and the constraints they pin:
//! - `singleEvents=true` makes the server expand recurring masters into
//!   one row per occurrence, each with a stable `{id}_{timestamp}` id,
//!   so an occurrence's dedup id survives re-polls. The client side
//!   never evaluates a recurrence rule.
//! - `timeMin`/`timeMax` bound the poll server-side. Google's filter is
//!   overlap-based, so the adapter still clips client-side to the
//!   window's own contract: starts within `[from, to)`.
//! - Rows with `status: cancelled` are dropped. On a singleEvents poll
//!   a cancelled row is the tombstone of an occurrence that will not
//!   happen; keeping it would remind about a meeting that was called
//!   off.
//! - Pagination rides `pageToken` until it disappears, with a hard
//!   page cap, so a server that keeps issuing cursors is declared
//!   misbehaving rather than polled forever.
//! - `startTime` ordering plus `singleEvents` is the only ordering
//!   Google allows over expanded instances, which is the order the
//!   normalized answer promises anyway (oldest first, id-broken ties).

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::calendar::{Calendar, finish};
use crate::error::{CalendarError, ensure_success, required};
use crate::event::{CalendarEvent, Window};

/// The production Google APIs base.
pub const DEFAULT_BASE_URL: &str = "https://www.googleapis.com";

/// The user agent the adapter presents.
const USER_AGENT: &str = concat!("rhema-calendar-integration/", env!("CARGO_PKG_VERSION"));

/// Rows requested per page: Google's own maximum.
const MAX_RESULTS: &str = "250";

/// Pages fetched before a server that keeps issuing cursors is
/// declared misbehaving rather than polled forever.
const MAX_PAGES: usize = 40;

/// Which credential a client authenticates with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Credential {
    /// An OAuth access token, in `Authorization: Bearer`.
    Bearer(String),
    /// An API key for public calendars, as the `key` parameter
    /// Google requires.
    ApiKey(String),
}

/// Google Calendar backend: a credential, a calendar id, one reqwest
/// client. The client is bound to one calendar, which is what the
/// [`Calendar`] contract promises anyway.
pub struct GoogleCalendar {
    base_url: String,
    calendar_id: String,
    credential: Credential,
    http: reqwest::Client,
}

impl GoogleCalendar {
    /// A calendar reading with an OAuth access `token` (private and
    /// public calendars both).
    pub fn new(token: impl Into<String>, calendar_id: impl Into<String>) -> Self {
        Self::with(Credential::Bearer(token.into()), calendar_id)
    }

    /// A calendar reading with an API `key`: public calendars only,
    /// Google's rule, and the key rides the `key` query parameter
    /// because that is the only form Google accepts for it.
    pub fn with_api_key(key: impl Into<String>, calendar_id: impl Into<String>) -> Self {
        Self::with(Credential::ApiKey(key.into()), calendar_id)
    }

    fn with(credential: Credential, calendar_id: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt: a dead endpoint fails fast instead
            // of stalling the poll loop.
            .connect_timeout(Duration::from_secs(10))
            // Every API call is a bounded request/response, so one
            // overall timeout per request is the right guard.
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            calendar_id: calendar_id.into(),
            credential,
            http,
        }
    }

    /// Overrides the base URL (trailing slashes trimmed) — for a test
    /// against a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// The events-list URL, with the calendar id percent-encoded by
    /// the URL builder: ids are email-shaped (`primary`, an address,
    /// a hex id), but the encoding is the builder's job, not ours.
    fn endpoint(&self) -> Result<reqwest::Url, CalendarError> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .map_err(|err| CalendarError::Parse(format!("base url: {err}")))?;
        url.path_segments_mut()
            .map_err(|_| CalendarError::Parse("base url is not a directory".to_owned()))?
            .extend(["calendar", "v3", "calendars", &self.calendar_id, "events"]);
        Ok(url)
    }
}

#[async_trait]
impl Calendar for GoogleCalendar {
    async fn events(&self, window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
        let time_min = window
            .from
            .format(&Rfc3339)
            .map_err(|err| CalendarError::Parse(format!("window start: {err}")))?;
        let time_max = window
            .to
            .format(&Rfc3339)
            .map_err(|err| CalendarError::Parse(format!("window end: {err}")))?;
        let mut rows = Vec::new();
        let mut page_token = String::new();
        for _page in 1..=MAX_PAGES {
            let mut url = self.endpoint()?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("timeMin", &time_min);
                query.append_pair("timeMax", &time_max);
                query.append_pair("singleEvents", "true");
                query.append_pair("orderBy", "startTime");
                query.append_pair("maxResults", MAX_RESULTS);
                if let Credential::ApiKey(key) = &self.credential {
                    query.append_pair("key", key);
                }
                if !page_token.is_empty() {
                    query.append_pair("pageToken", &page_token);
                }
            }
            let mut request = self.http.get(url);
            if let Credential::Bearer(token) = &self.credential {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(CalendarError::network)?;
            let body = ensure_success(response)
                .await?
                .text()
                .await
                .map_err(CalendarError::network)?;
            let page: EventsPage = serde_json::from_str(&body)
                .map_err(|err| CalendarError::Parse(format!("events page: {err}")))?;
            rows.extend(page.items);
            match page.next_page_token {
                Some(token) => page_token = token,
                None => return Ok(finish(try_rows(rows)?, window)),
            }
        }
        Err(CalendarError::Pagination(format!(
            "more than {MAX_PAGES} pages from the events list"
        )))
    }
}

/// One events-list page: the raw rows travel as JSON so each can keep
/// riding its event's `raw` untouched.
#[derive(Debug, Deserialize)]
struct EventsPage {
    #[serde(default)]
    items: Vec<Value>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

/// Decodes every row of a page, dropping the tombstones.
fn try_rows(rows: Vec<Value>) -> Result<Vec<CalendarEvent>, CalendarError> {
    rows.into_iter()
        .filter_map(|row| decode_row(row).transpose())
        .collect()
}

/// Decodes one wire row; `None` for a cancelled tombstone. A missing
/// start is loud: without it there is no instant to remind about.
fn decode_row(row: Value) -> Result<Option<CalendarEvent>, CalendarError> {
    let row: EventRow = serde_json::from_value(row)
        .map_err(|err| CalendarError::Parse(format!("event row: {err}")))?;
    if row.status.as_deref() == Some("cancelled") {
        return Ok(None);
    }
    let start_block = required(row.start, "a start")?;
    let (start, all_day) = start_block.read(&row.id)?;
    let end = match row.end_time_unspecified {
        true => None,
        false => row
            .end
            .as_ref()
            .map(|end| end.read(&row.id))
            .transpose()?
            .map(|(when, _)| when),
    };
    Ok(Some(CalendarEvent {
        id: format!("google:{}@{}", row.id, start.unix_timestamp()),
        title: row.summary.unwrap_or_default(),
        start,
        end,
        all_day,
        location: row.location,
        description: row.description,
        attendees: row
            .attendees
            .unwrap_or_default()
            .into_iter()
            .filter_map(|attendee| attendee.email)
            .collect(),
        organizer: row.organizer.and_then(|organizer| organizer.email),
        url: row.html_link,
        raw: row.raw,
    }))
}

/// A wire time block: `dateTime` for a timed row, `date` for an
/// all-day one (that date's midnight UTC).
#[derive(Debug, Deserialize)]
struct TimeBlock {
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
}

impl TimeBlock {
    /// The instant plus the all-day flag; `None` when the block names
    /// neither form. An unreadable spelling is loud, with the row's id
    /// for context.
    fn read(&self, id: &str) -> Result<(OffsetDateTime, bool), CalendarError> {
        let loud = |what: &str, err: String| {
            CalendarError::Parse(format!("event {id:?} has an unreadable {what}: {err}"))
        };
        if let Some(when) = self.date.as_deref() {
            // `YYYY-MM-DD`, at midnight UTC.
            let (year, rest) = when
                .split_once('-')
                .ok_or_else(|| loud("date", "no year separator".to_owned()))?;
            let (month, day) = rest
                .split_once('-')
                .ok_or_else(|| loud("date", "no month separator".to_owned()))?;
            let date = time::Date::from_calendar_date(
                year.parse::<i32>()
                    .map_err(|err| loud("date year", err.to_string()))?,
                time::Month::try_from(
                    month
                        .parse::<u8>()
                        .map_err(|err| loud("date month", err.to_string()))?,
                )
                .map_err(|err| loud("date month", err.to_string()))?,
                day.parse::<u8>()
                    .map_err(|err| loud("date day", err.to_string()))?,
            )
            .map_err(|err| loud("date", err.to_string()))?;
            return Ok((date.with_time(time::Time::MIDNIGHT).assume_utc(), true));
        }
        let when = self
            .date_time
            .as_deref()
            .ok_or_else(|| loud("start", "neither date nor dateTime".to_owned()))?;
        OffsetDateTime::parse(when, &Rfc3339)
            .map(|start| (start, false))
            .map_err(|err| loud("dateTime", err.to_string()))
    }
}

/// The wire row, with the fields the vocabulary lifts named.
#[derive(Debug, Deserialize)]
struct EventRow {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "htmlLink", default)]
    html_link: Option<String>,
    #[serde(default)]
    start: Option<TimeBlock>,
    #[serde(default)]
    end: Option<TimeBlock>,
    #[serde(rename = "endTimeUnspecified", default)]
    end_time_unspecified: bool,
    #[serde(default)]
    attendees: Option<Vec<Party>>,
    #[serde(default)]
    organizer: Option<Party>,
    /// Kept whole for [`CalendarEvent::raw`].
    #[serde(flatten)]
    raw: Value,
}

/// An attendee or organizer row; only the address is lifted.
#[derive(Debug, Deserialize)]
struct Party {
    #[serde(default)]
    email: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn timed(id: &str, start: &str) -> Value {
        json!({
            "id": id,
            "status": "confirmed",
            "summary": "standup",
            "start": { "dateTime": start },
            "end": { "dateTime": "2026-10-20T09:15:00Z" },
            "htmlLink": "https://calendar.google.com/x",
            "attendees": [ { "email": "a@example.net" }, { "displayName": "no address" } ]
        })
    }

    #[test]
    fn a_timed_row_decodes_with_its_lifts() {
        let event = decode_row(timed("abc123", "2026-10-20T09:00:00Z"))
            .expect("decodes")
            .expect("not cancelled");
        assert_eq!(
            event.id,
            format!(
                "google:abc123@{}",
                OffsetDateTime::parse("2026-10-20T09:00:00Z", &Rfc3339)
                    .expect("parses")
                    .unix_timestamp()
            )
        );
        assert_eq!(event.title, "standup");
        assert!(!event.all_day);
        assert_eq!(event.attendees, vec!["a@example.net".to_owned()]);
        assert_eq!(event.url.as_deref(), Some("https://calendar.google.com/x"));
    }

    #[test]
    fn an_all_day_row_starts_at_its_midnight() {
        let row = json!({
            "id": "bday",
            "summary": "birthday",
            "start": { "date": "2026-10-20" }
        });
        let event = decode_row(row).expect("decodes").expect("not cancelled");
        assert!(event.all_day);
        assert_eq!(event.end, None);
        assert_eq!(event.start.hour(), 0);
    }

    #[test]
    fn a_cancelled_row_is_a_tombstone() {
        let mut row = timed("abc123", "2026-10-20T09:00:00Z");
        row["status"] = json!("cancelled");
        assert_eq!(decode_row(row).expect("decodes"), None);
    }

    #[test]
    fn a_row_without_a_start_is_loud() {
        let mut row = timed("abc123", "2026-10-20T09:00:00Z");
        row["start"] = json!({});
        let err = decode_row(row).expect_err("loud");
        assert!(err.to_string().contains("start"), "got: {err}");
    }
}
