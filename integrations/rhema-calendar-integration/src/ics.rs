//! The ICS adapter: one HTTP GET of an iCalendar feed, expanded.
//!
//! The workhorse backend: Google Calendar's "secret address" (the
//! private iCal export), Nextcloud, Radicale, Fastmail, and most other
//! servers publish one URL that speaks it. Auth, when a feed demands
//! it, is the token in `Authorization: Bearer`; the token never lands
//! in a URL or a log line.
//!
//! Wire choices and the constraints they pin:
//! - Only `VEVENT` components count; `VTODO`/`VJOURNAL`/`VFREEBUSY`
//!   are not reminders. Nested components inside an event (`VALARM`)
//!   are skipped, not mistaken for properties.
//! - A feed carries master components plus overrides. The adapter
//!   expands a master's `RRULE` client-side and applies
//!   `RECURRENCE-ID` overrides on top: an override replaces the
//!   occurrence at the instant it names (a cancelled one removes it),
//!   and one with no master occurrence stands alone. `EXDATE` rows
//!   drop occurrences the same way.
//! - The expansion support matrix, deliberately small: `FREQ` daily,
//!   weekly (with `BYDAY`), monthly (with `BYMONTHDAY`, else
//!   `DTSTART`'s day), yearly; `INTERVAL`; `COUNT`; `UNTIL`. Anything
//!   else (`SECONDLY` and finer, `BYSETPOS`, `BYDAY` in a
//!   monthly/yearly rule, `BYMONTH`, `WKST` other than the default
//!   `MO`) degrades to the `DTSTART` occurrence only, so a rule the
//!   adapter cannot expand still surfaces its first instance rather
//!   than silently emptying the calendar.
//! - Text values are unescaped (`\n`, `\,`, `\;`); property names and
//!   parameters are case-insensitive; continuation lines are unfolded;
//!   quoted parameter values may contain colons.
//! - A floating time (no `Z`) reads as UTC, and a `TZID` parameter
//!   rides [`CalendarEvent::raw`] for a consumer that can resolve
//!   zones. All-day rows (`VALUE=DATE` or a bare date) start at their
//!   midnight UTC. RFC 3339 spellings are accepted too, because some
//!   exporters emit them.
//! - `STATUS:CANCELLED` rows are dropped. A row without `UID` or
//!   `DTSTART` is a loud parse error: feeds are machine-generated,
//!   and a shape break must surface, not shrink the calendar.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use time::Date;
use time::Month;
use time::OffsetDateTime;
use time::Time;
use time::Weekday;
use time::format_description::well_known::Rfc3339;

use crate::calendar::{Calendar, finish};
use crate::error::{CalendarError, ensure_success, required};
use crate::event::{CalendarEvent, Window};

/// The user agent the adapter presents; feeds log it, and some reject
/// requests without one.
const USER_AGENT: &str = concat!("rhema-calendar-integration/", env!("CARGO_PKG_VERSION"));

/// The most occurrence candidates one expansion may feed before the
/// walk is declared pathological rather than continued. `COUNT` and
/// the window bound every well-formed rule; this cap bounds the rest.
const MAX_CANDIDATES: u64 = 100_000;

/// ICS feed backend: a URL, an optional bearer token, one reqwest
/// client. An empty token polls anonymous, which is how most published
/// feed URLs work (the secret rides the URL itself).
pub struct IcsCalendar {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl IcsCalendar {
    /// A feed at `url` (an `https://` or `webcal://` address; webcal
    /// is fetched as https).
    pub fn new(url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt: a dead feed fails fast instead of
            // stalling the poll loop.
            .connect_timeout(Duration::from_secs(10))
            // Every feed call is a bounded request/response, so one
            // overall timeout per request is the right guard.
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            url: normalize_url(url.into()),
            token: String::new(),
            http,
        }
    }

    /// Authenticates feeds that demand it: the token rides the
    /// `Authorization: Bearer` header only.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = token.into();
        self
    }
}

/// `webcal://` is iCalendar over https by convention; normalize it so
/// the reqwest client never sees a scheme it cannot speak.
fn normalize_url(url: String) -> String {
    let trimmed = url.trim();
    trimmed
        .strip_prefix("webcal://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| trimmed.to_owned())
}

#[async_trait]
impl Calendar for IcsCalendar {
    async fn events(&self, window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
        let mut request = self.http.get(&self.url);
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        let response = request.send().await.map_err(CalendarError::network)?;
        let body = ensure_success(response)
            .await?
            .text()
            .await
            .map_err(CalendarError::network)?;
        expand(&body, window)
    }
}

/// One content line: `NAME;PARAM=value:value`, names and parameters
/// uppercased, the value untouched (unescaping is a text property's
/// business, not the line's).
#[derive(Debug, Clone)]
struct ContentLine {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl ContentLine {
    /// A parameter's value, compared case-insensitively on the name.
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A run of ASCII digits at `range`, as its number.
fn digits(raw: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let slice = raw.get(range)?;
    if slice.is_empty() || !slice.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    slice.parse().ok()
}

/// Splits one unfolded line at the first colon outside double quotes,
/// so a quoted parameter value (`CN="Smith: Boss"`) cannot split the
/// line in half.
fn split_content_line(line: &str) -> Option<ContentLine> {
    let mut quoted = false;
    let mut split_at = None;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                split_at = Some(index);
                break;
            }
            _ => {}
        }
    }
    let split_at = split_at?;
    let head = &line[..split_at];
    let value = line[split_at + 1..].to_owned();
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((
                key.trim().to_ascii_uppercase(),
                value.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect();
    Some(ContentLine {
        name,
        params,
        value,
    })
}

/// Unfolds a feed: a line starting with a single space or tab
/// continues the previous one, per RFC 5545 §3.1.
fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw_line in raw.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            // A continuation with nothing to continue is skipped.
            if let Some(last) = lines.last_mut() {
                last.push_str(&raw_line[1..]);
            }
        } else {
            lines.push(raw_line.to_owned());
        }
    }
    lines
}

/// Walks the component stack and collects each `VEVENT`'s own lines:
/// nested components (`VALARM`) inside an event contribute nothing.
fn vevent_blocks(raw: &str) -> Vec<Vec<ContentLine>> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<ContentLine>> = None;
    let mut stack: Vec<String> = Vec::new();
    for line in unfold(raw) {
        let Some(line) = split_content_line(&line) else {
            continue;
        };
        match line.name.as_str() {
            "BEGIN" => {
                let component = line.value.trim().to_ascii_uppercase();
                let is_event = component == "VEVENT";
                stack.push(component);
                if is_event && current.is_none() {
                    current = Some(Vec::new());
                }
            }
            "END" => {
                let component = line.value.trim().to_ascii_uppercase();
                if component == "VEVENT" {
                    if let Some(block) = current.take() {
                        blocks.push(block);
                    }
                }
                stack.pop();
            }
            _ => {
                if stack.last().map(String::as_str) == Some("VEVENT") {
                    if let Some(block) = current.as_mut() {
                        block.push(line);
                    }
                }
            }
        }
    }
    blocks
}

/// Unescapes a text value: the four RFC 5545 escapes, nothing more.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            Some('\\') => out.push('\\'),
            // An unknown escape keeps both characters: the feed's
            // bytes, not this parser's guess, survive.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Parses iCalendar's compact datetime, `YYYYMMDDTHHMMSS` with an
/// optional trailing `Z`. The `Z` decides nothing here: both forms
/// read as UTC (a floating time is the feed saying "UTC or bust" to a
/// reader without a zone database).
fn parse_compact(value: &str) -> Option<OffsetDateTime> {
    let bytes = value.as_bytes();
    if bytes.len() != 15 && bytes.len() != 16 {
        return None;
    }
    if bytes[8] != b'T' && bytes[8] != b't' {
        return None;
    }
    if bytes.len() == 16 && !(bytes[15] == b'Z' || bytes[15] == b'z') {
        return None;
    }
    let year = digits(value, 0..4)?;
    let month = digits(value, 4..6)?;
    let day = digits(value, 6..8)?;
    let hour = digits(value, 9..11)?;
    let minute = digits(value, 11..13)?;
    let second = digits(value, 13..15)?;
    let date = Date::from_calendar_date(year as i32, Month::try_from(month as u8).ok()?, day as u8)
        .ok()?;
    let clock = Time::from_hms(hour as u8, minute as u8, second as u8).ok()?;
    Some(date.with_time(clock).assume_utc())
}

/// Parses an iCalendar date, `YYYYMMDD`, as its midnight UTC.
fn parse_date(value: &str) -> Option<OffsetDateTime> {
    if value.len() != 8 {
        return None;
    }
    let year = digits(value, 0..4)?;
    let month = digits(value, 4..6)?;
    let day = digits(value, 6..8)?;
    let date = Date::from_calendar_date(year as i32, Month::try_from(month as u8).ok()?, day as u8)
        .ok()?;
    Some(date.with_time(Time::MIDNIGHT).assume_utc())
}

/// Parses any `DTSTART`-shape value into an instant plus its all-day
/// flag. Accepts `VALUE=DATE`, the compact forms, a bare date, and an
/// RFC 3339 spelling (some exporters emit one).
fn parse_when(line: &ContentLine) -> Option<(OffsetDateTime, bool)> {
    let value = line.value.trim();
    if line.param("VALUE").map(str::to_ascii_uppercase).as_deref() == Some("DATE") {
        return parse_date(value).map(|start| (start, true));
    }
    if let Some(start) = parse_compact(value) {
        return Some((start, false));
    }
    if let Some(start) = parse_date(value) {
        return Some((start, true));
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|start| (start, false))
}

/// Parses an iCalendar duration, `[+-]P[nW][nD][T[nH][nM][nS]]`.
/// Negative and unparsable values read as none: a duration is a
/// nicety on an end-less row, never worth failing a feed over.
fn parse_duration(value: &str) -> Option<time::Duration> {
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P').or_else(|| rest.strip_prefix('p'))?;
    let mut weeks = 0i64;
    let mut days = 0i64;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut in_time = false;
    let mut number = String::new();
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        // The T introducing the time part arrives with no number
        // before it; every unit letter must follow one.
        if !number.is_empty() {
            let Ok(amount) = number.parse::<i64>() else {
                return None;
            };
            number.clear();
            match ch {
                'W' if !in_time => weeks = amount,
                'D' if !in_time => days = amount,
                'H' if in_time => hours = amount,
                'M' if in_time => minutes = amount,
                'S' if in_time => seconds = amount,
                _ => return None,
            }
        } else if matches!(ch, 'T' | 't') && !in_time {
            in_time = true;
        } else {
            return None;
        }
    }
    if !number.is_empty() {
        return None;
    }
    let total = weeks * 604_800 + days * 86_400 + hours * 3600 + minutes * 60 + seconds;
    let duration = time::Duration::seconds(if negative { -total } else { total });
    (duration > time::Duration::ZERO).then_some(duration)
}

/// Which cadence a rule repeats on. `Unsupported` is the honest
/// spelling for every shape the expansion matrix leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Unsupported,
}

/// One `RRULE`, reduced to the shapes feeds emit.
#[derive(Debug, Clone)]
struct Rrule {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until: Option<OffsetDateTime>,
    by_day: Vec<Weekday>,
    by_month_day: Vec<u8>,
}

/// Parses an `RRULE` value. Any part outside the support matrix marks
/// the whole rule unsupported, and an unparsable piece degrades to its
/// default rather than failing the feed: an expansion that guesses is
/// worse than an expansion that declines.
fn parse_rrule(value: &str) -> Rrule {
    let mut freq = Freq::Unsupported;
    let mut interval = 1u32;
    let mut count = None;
    let mut until = None;
    let mut by_day = Vec::new();
    let mut by_month_day = Vec::new();
    let mut unsupported = false;
    for part in value.trim().split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = match value.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => Freq::Unsupported,
                };
            }
            "INTERVAL" => {
                interval = value.trim().parse().unwrap_or(1).max(1);
            }
            "COUNT" => count = value.trim().parse().ok(),
            "UNTIL" => {
                let line = ContentLine {
                    name: String::new(),
                    params: Vec::new(),
                    value: value.trim().to_owned(),
                };
                until = parse_when(&line).map(|(when, _)| when);
            }
            "BYDAY" => {
                for item in value.trim().split(',') {
                    // An ordinal prefix (`1MO`, `-1FR`) is the
                    // nth-weekday-of-month shape the matrix leaves out.
                    let bare = item.trim().trim_start_matches(|ch: char| {
                        ch.is_ascii_digit() || ch == '-' || ch == '+'
                    });
                    if bare.len() != item.trim().len() {
                        unsupported = true;
                    }
                    match bare {
                        "MO" => by_day.push(Weekday::Monday),
                        "TU" => by_day.push(Weekday::Tuesday),
                        "WE" => by_day.push(Weekday::Wednesday),
                        "TH" => by_day.push(Weekday::Thursday),
                        "FR" => by_day.push(Weekday::Friday),
                        "SA" => by_day.push(Weekday::Saturday),
                        "SU" => by_day.push(Weekday::Sunday),
                        _ => unsupported = true,
                    }
                }
            }
            "BYMONTHDAY" => {
                for item in value.trim().split(',') {
                    let item = item.trim();
                    match item.parse::<u8>() {
                        Ok(day) if (1..=31).contains(&day) => by_month_day.push(day),
                        // Negative ordinals count from the month's end;
                        // the matrix leaves them out.
                        _ => unsupported = true,
                    }
                }
            }
            // BYSETPOS, BYMONTH, BYWEEKNO, BYYEARDAY, WKST, and every
            // unknown part: unsupported, loudly, by the same rule.
            _ => unsupported = true,
        }
    }
    if unsupported {
        freq = Freq::Unsupported;
    }
    Rrule {
        freq,
        interval,
        count,
        until,
        by_day,
        by_month_day,
    }
}

/// One parsed `VEVENT`: the fields the vocabulary lifts, the rule and
/// override marks the expansion needs, and the template event the
/// occurrences clone.
struct Parsed {
    uid: String,
    start: OffsetDateTime,
    duration: time::Duration,
    template: CalendarEvent,
    status: Option<String>,
    recurrence_id: Option<OffsetDateTime>,
    exdates: Vec<i64>,
    rrule: Option<Rrule>,
}

/// Parses one `VEVENT` block. A row without `UID` or `DTSTART` fails
/// the feed loudly; everything else degrades politely.
fn parse_event(block: &[ContentLine]) -> Result<Parsed, CalendarError> {
    let first = |name: &str| block.iter().find(|line| line.name == name);
    let uid = required(first("UID"), "a UID")?.value.trim().to_owned();
    let dtstart = required(first("DTSTART"), "a DTSTART")?;
    let (start, all_day) = parse_when(dtstart)
        .ok_or_else(|| CalendarError::Parse(format!("event {uid:?} has an unreadable DTSTART")))?;

    // The end rides DTEND when present, else DURATION, else nothing.
    // A negative span is the feed's typo, not ours to widen: it reads
    // as zero.
    let end = first("DTEND")
        .and_then(parse_when)
        .map(|(when, _)| when)
        .or_else(|| {
            first("DURATION")
                .and_then(|line| parse_duration(&line.value))
                .and_then(|duration| start.checked_add(duration))
        });
    let duration = end
        .map(|end| (end - start).max(time::Duration::ZERO))
        .unwrap_or(time::Duration::ZERO);

    let text = |name: &str| first(name).map(|line| unescape(&line.value));
    // `mailto:` is the scheme ICS addresses ride; strip it wherever it
    // appears, case-insensitively.
    let bare_address = |value: &str| match value.trim() {
        trimmed if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("mailto:") => {
            trimmed[7..].trim().to_owned()
        }
        trimmed => trimmed.to_owned(),
    };

    let mut raw = Map::new();
    for line in block {
        let mut key = line.name.clone();
        if !line.params.is_empty() {
            key.push(';');
            key.push_str(
                &line
                    .params
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(";"),
            );
        }
        use serde_json::map::Entry;
        match raw.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(Value::String(line.value.clone()));
            }
            Entry::Occupied(mut slot) => match slot.get_mut() {
                // A repeated property (ATTENDEE, for one) becomes an
                // array on its second occurrence.
                Value::Array(items) => items.push(Value::String(line.value.clone())),
                existing => {
                    let first = existing.take();
                    *slot.get_mut() = Value::Array(vec![first, Value::String(line.value.clone())]);
                }
            },
        }
    }

    let template = CalendarEvent {
        id: format!("ics:{uid}@{}", start.unix_timestamp()),
        title: text("SUMMARY").unwrap_or_default(),
        start,
        end,
        all_day,
        location: text("LOCATION"),
        description: text("DESCRIPTION"),
        attendees: block
            .iter()
            .filter(|line| line.name == "ATTENDEE")
            .map(|line| bare_address(&line.value))
            .collect(),
        organizer: first("ORGANIZER").map(|line| bare_address(&line.value)),
        url: text("URL"),
        raw: Value::Object(raw),
    };

    Ok(Parsed {
        uid,
        start,
        duration,
        template,
        status: first("STATUS").map(|line| line.value.trim().to_ascii_uppercase()),
        recurrence_id: first("RECURRENCE-ID")
            .and_then(parse_when)
            .map(|(when, _)| when),
        exdates: first("EXDATE")
            .map(|line| {
                line.value
                    .split(',')
                    .map(|value| {
                        let line = ContentLine {
                            name: String::new(),
                            params: line.params.clone(),
                            value: value.to_owned(),
                        };
                        parse_when(&line).map(|(when, _)| when.unix_timestamp())
                    })
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default()
            })
            .unwrap_or_default(),
        rrule: first("RRULE").map(|line| parse_rrule(&line.value)),
    })
}

/// Expands a whole feed: parse, expand the masters, apply the
/// overrides, keep what starts in `window`.
pub(crate) fn expand(raw: &str, window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
    let mut singles = Vec::new();
    let mut masters = Vec::new();
    let mut overrides: HashMap<i64, Parsed> = HashMap::new();
    // A cancelled override is the tombstone of one occurrence: its
    // slot suppresses the master occurrence, and nothing answers for
    // it. A cancelled master takes its whole rule with it.
    let mut cancelled_slots: HashSet<i64> = HashSet::new();
    for block in vevent_blocks(raw) {
        let parsed = parse_event(&block)?;
        if parsed.status.as_deref() == Some("CANCELLED") {
            if let Some(recurrence_id) = parsed.recurrence_id {
                cancelled_slots.insert(recurrence_id.unix_timestamp());
            }
            continue;
        }
        match parsed.recurrence_id {
            Some(recurrence_id) => {
                overrides.insert(recurrence_id.unix_timestamp(), parsed);
            }
            None if parsed.rrule.is_some() => masters.push(parsed),
            None => singles.push(parsed),
        }
    }

    let mut suppress_all: HashSet<i64> = overrides.keys().copied().collect();
    suppress_all.extend(cancelled_slots);
    let mut events = Vec::new();
    for master in &masters {
        let mut suppress = suppress_all.clone();
        suppress.extend(master.exdates.iter().copied());
        expand_rule(master, window, &suppress, &mut events);
    }
    // Every override answers with its own occurrence, moved start
    // included; the master occurrence it names is the one dropped.
    // One whose master never generated its slot stands alone, which is
    // the only reading a broken feed leaves open.
    for parsed in overrides.into_values() {
        events.push(parsed.template);
    }
    for single in singles {
        events.push(single.template);
    }
    Ok(finish(events, window))
}

/// One occurrence of a master: the template with the occurrence's own
/// start, end, and id.
fn occurrence(parsed: &Parsed, start: OffsetDateTime) -> CalendarEvent {
    let mut event = parsed.template.clone();
    event.id = format!("ics:{}@{}", parsed.uid, start.unix_timestamp());
    event.start = start;
    event.end = start.checked_add(parsed.duration);
    event
}

/// Whole days since the epoch, UTC: the ruler every interval is
/// counted against.
fn day_number(date: Date) -> i64 {
    date.with_time(Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp()
        .div_euclid(86_400)
}

/// Whole weeks since the epoch's Monday (1970-01-05), Monday-based:
/// the invariant weekly `INTERVAL`s are counted against.
fn week_number(date: Date) -> i64 {
    (day_number(date) - 4).div_euclid(7)
}

/// `year`/`month` shifted by `months`, out-of-range days handled by
/// the caller (a February 30th is a month without that day, not an
/// error).
fn add_months(year: i32, month: Month, months: i64) -> (i32, Month) {
    let total = year as i64 * 12 + i64::from(u8::from(month)) - 1 + months;
    (
        total.div_euclid(12) as i32,
        Month::try_from((total.rem_euclid(12) + 1) as u8).expect("month is 1..=12"),
    )
}

/// Expands one master's rule into `events`, candidates in
/// chronological order. A candidate an override or `EXDATE` claims is
/// walked (so `COUNT` stays true to the feed) but not emitted: the
/// claimant answers for it instead.
fn expand_rule(
    parsed: &Parsed,
    window: &Window,
    suppress: &HashSet<i64>,
    events: &mut Vec<CalendarEvent>,
) {
    let Some(rule) = parsed.rrule.as_ref() else {
        return;
    };
    let mut count_seen = 0u64;
    let mut walked = 0u64;
    let limit = rule.count.map(u64::from).unwrap_or(u64::MAX);

    // Feeds one candidate; false when the walk must stop. Candidates
    // arrive in chronological order, so the first one past `until` or
    // the window's end ends it, and `COUNT` counts occurrences, not
    // walk steps. The walk cap is the pathological-feed guard: every
    // well-formed rule is already bounded by `COUNT`, `until`, or the
    // window itself.
    let mut candidate = |start: OffsetDateTime| -> bool {
        walked += 1;
        if walked > MAX_CANDIDATES {
            return false;
        }
        if let Some(until) = rule.until {
            if start > until {
                return false;
            }
        }
        if start > window.to {
            return false;
        }
        if count_seen >= limit {
            return false;
        }
        count_seen += 1;
        if !suppress.contains(&start.unix_timestamp()) && window.starts_within(start) {
            events.push(occurrence(parsed, start));
        }
        count_seen < limit
    };

    match rule.freq {
        Freq::Unsupported => {
            // The honest degradation: the first instance only.
            if window.starts_within(parsed.start)
                && !suppress.contains(&parsed.start.unix_timestamp())
            {
                events.push(occurrence(parsed, parsed.start));
            }
        }
        Freq::Daily | Freq::Weekly => {
            let start_week = week_number(parsed.start.date());
            let start_day = day_number(parsed.start.date());
            let mut day = parsed.start.date();
            loop {
                let on_rule = match rule.freq {
                    Freq::Weekly => {
                        let weekday_ok =
                            rule.by_day.is_empty() || rule.by_day.contains(&day.weekday());
                        let week_ok =
                            (week_number(day) - start_week) % i64::from(rule.interval) == 0;
                        weekday_ok && week_ok
                    }
                    _ => (day_number(day) - start_day) % i64::from(rule.interval) == 0,
                };
                if !on_rule {
                    let Some(next) = day.next_day() else {
                        break;
                    };
                    day = next;
                    if day > window.to.date() {
                        break;
                    }
                    continue;
                }
                let start = day.with_time(parsed.start.time()).assume_utc();
                if !candidate(start) {
                    break;
                }
                let Some(next) = day.next_day() else {
                    break;
                };
                day = next;
                if day > window.to.date() {
                    break;
                }
            }
        }
        Freq::Monthly => {
            let days = if rule.by_month_day.is_empty() {
                vec![parsed.start.day()]
            } else {
                let mut days = rule.by_month_day.clone();
                days.sort_unstable();
                days
            };
            let mut month = 0i64;
            loop {
                let (year, this_month) = add_months(
                    parsed.start.year(),
                    parsed.start.month(),
                    month * i64::from(rule.interval),
                );
                let Ok(first) = Date::from_calendar_date(year, this_month, 1) else {
                    break;
                };
                if first.with_time(Time::MIDNIGHT).assume_utc() > window.to {
                    break;
                }
                month += 1;
                for day in &days {
                    if let Ok(date) = Date::from_calendar_date(year, this_month, *day) {
                        let start = date.with_time(parsed.start.time()).assume_utc();
                        if !candidate(start) {
                            return;
                        }
                    }
                    // A month without that day (a 31st in February)
                    // contributes no occurrence, by the RFC's own rule.
                }
            }
        }
        Freq::Yearly => {
            let mut year = 0i64;
            loop {
                let this_year = parsed.start.year() + (year * i64::from(rule.interval)) as i32;
                year += 1;
                let Ok(january) = Date::from_calendar_date(this_year, Month::January, 1) else {
                    break;
                };
                if january.with_time(Time::MIDNIGHT).assume_utc() > window.to {
                    break;
                }
                // A year without that day (a February 29th) is
                // skipped, not clamped.
                if let Ok(date) =
                    Date::from_calendar_date(this_year, parsed.start.month(), parsed.start.day())
                {
                    let start = date.with_time(parsed.start.time()).assume_utc();
                    if !candidate(start) {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start: &str, hours: i64) -> Window {
        let from = OffsetDateTime::parse(start, &Rfc3339).expect("window start parses");
        Window::new(from, from + time::Duration::hours(hours))
    }

    fn ids(events: &[CalendarEvent]) -> Vec<String> {
        events.iter().map(|event| event.id.clone()).collect()
    }

    #[test]
    fn webcal_urls_normalize_to_https() {
        assert_eq!(
            normalize_url("webcal://example.net/cal.ics".to_owned()),
            "https://example.net/cal.ics"
        );
        assert_eq!(
            normalize_url("  https://example.net/cal.ics ".to_owned()),
            "https://example.net/cal.ics"
        );
    }

    #[test]
    fn unfolding_joins_continuation_lines() {
        let raw = "SUMMARY:a long\r\n title\r\nLOCATION:paris\n , france\nEND:VEVENT";
        assert_eq!(
            unfold(raw),
            vec![
                "SUMMARY:a longtitle".to_owned(),
                "LOCATION:paris, france".to_owned(),
                "END:VEVENT".to_owned(),
            ]
        );
    }

    #[test]
    fn a_quoted_parameter_may_carry_a_colon() {
        let line =
            split_content_line(r#"ATTENDEE;CN="Smith: Boss";RSVP=TRUE:mailto:bob@example.net"#)
                .expect("parses");
        assert_eq!(line.name, "ATTENDEE");
        assert_eq!(line.param("CN"), Some(r#"Smith: Boss"#));
        assert_eq!(line.value, "mailto:bob@example.net");
    }

    #[test]
    fn compact_datetimes_and_dates_read_as_utc() {
        let zulu = parse_compact("20261020T140000Z").expect("zulu parses");
        let floating = parse_compact("20261020T140000").expect("floating parses");
        assert_eq!(zulu, floating);
        let date = parse_date("20261020").expect("date parses");
        assert_eq!(date, zulu.replace_time(Time::MIDNIGHT));
        let rfc = parse_when(&ContentLine {
            name: "DTSTART".into(),
            params: Vec::new(),
            value: "2026-10-20T14:00:00Z".into(),
        })
        .expect("rfc 3339 parses");
        assert_eq!(rfc.0, zulu);
    }

    #[test]
    fn durations_parse_in_every_unit() {
        assert_eq!(parse_duration("PT1H30M"), Some(time::Duration::minutes(90)));
        assert_eq!(parse_duration("P1D"), Some(time::Duration::days(1)));
        assert_eq!(parse_duration("P2W"), Some(time::Duration::days(14)));
        assert_eq!(parse_duration("PT15M"), Some(time::Duration::minutes(15)));
        assert_eq!(parse_duration("-PT15M"), None, "negative reads as none");
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn a_single_event_expands_to_itself() {
        let raw = "BEGIN:VEVENT\nUID:a@x\nDTSTART:20261020T140000Z\nDTEND:20261020T150000Z\n\
                   SUMMARY:one-off\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        let at = OffsetDateTime::parse("2026-10-20T14:00:00Z", &Rfc3339).expect("parses");
        assert_eq!(
            ids(&events),
            vec![format!("ics:a@x@{}", at.unix_timestamp())]
        );
        assert_eq!(events[0].title, "one-off");
        assert_eq!(events[0].duration(), time::Duration::hours(1));
    }

    #[test]
    fn text_values_unescape_and_addresses_strip_mailto() {
        let raw = "BEGIN:VEVENT\nUID:a@x\nDTSTART:20261020T140000Z\n\
                   SUMMARY:quarterly\\, review\\n Agenda: TBA\n\
                   ORGANIZER;CN=Boss:MAILTO:boss@example.net\n\
                   ATTENDEE:mailto:alice@example.net\nATTENDEE:bob@example.net\n\
                   END:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert_eq!(events[0].title, "quarterly, review\n Agenda: TBA");
        assert_eq!(events[0].organizer.as_deref(), Some("boss@example.net"));
        assert_eq!(
            events[0].attendees,
            vec!["alice@example.net".to_owned(), "bob@example.net".to_owned()]
        );
    }

    #[test]
    fn a_weekly_byday_rule_expands_across_the_window() {
        let raw = "BEGIN:VEVENT\nUID:standup@x\nDTSTART:20261005T090000Z\nDTEND:20261005T093000Z\n\
                   RRULE:FREQ=WEEKLY;BYDAY=MO,WE\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-19T00:00:00Z", 24 * 7)).expect("expands");
        // The window is Mon Oct 19 .. Sun Oct 25: the rule lands on
        // Mon 19, Wed 21, Mon 26 is past `to`.
        assert_eq!(ids(&events).len(), 2);
        assert_eq!(
            events[0].start,
            OffsetDateTime::parse("2026-10-19T09:00:00Z", &Rfc3339).expect("parses")
        );
        assert_eq!(
            events[1].start,
            OffsetDateTime::parse("2026-10-21T09:00:00Z", &Rfc3339).expect("parses")
        );
    }

    #[test]
    fn a_biweekly_rule_counts_weeks_from_dtstart() {
        // DTSTART is Mon Oct 5; interval 2 puts Oct 19 in, Oct 12 out.
        let raw = "BEGIN:VEVENT\nUID:fortnight@x\nDTSTART:20261005T100000Z\n\
                   RRULE:FREQ=WEEKLY;INTERVAL=2\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-12T00:00:00Z", 24 * 9)).expect("expands");
        let starts: Vec<String> = events
            .iter()
            .map(|event| event.start.format(&Rfc3339).expect("formats"))
            .collect();
        assert!(
            starts
                .iter()
                .any(|start| start.contains("20261019") || start.contains("10-19"))
        );
        assert!(
            !starts
                .iter()
                .any(|start| start.contains("10-12") || start.contains("20261012"))
        );
    }

    #[test]
    fn count_bounds_the_expansion_even_past_the_window() {
        let raw = "BEGIN:VEVENT\nUID:thrice@x\nDTSTART:20261001T080000Z\n\
                   RRULE:FREQ=DAILY;COUNT=3\nEND:VEVENT\n";
        // The three occurrences are Oct 1..3; a mid-October window
        // sees none of them.
        let events = expand(raw, &window("2026-10-15T00:00:00Z", 24)).expect("expands");
        assert!(events.is_empty());
    }

    #[test]
    fn a_monthly_rule_skips_months_without_the_day() {
        let raw = "BEGIN:VEVENT\nUID:payday@x\nDTSTART:20260131T120000Z\n\
                   RRULE:FREQ=MONTHLY\nEND:VEVENT\n";
        // February has no 31st; the walk lands on Mar 31.
        let events = expand(raw, &window("2026-03-31T00:00:00Z", 24)).expect("expands");
        assert_eq!(ids(&events).len(), 1);
    }

    #[test]
    fn until_stops_the_rule() {
        let raw = "BEGIN:VEVENT\nUID:course@x\nDTSTART:20261001T180000Z\n\
                   RRULE:FREQ=DAILY;UNTIL=20261003T180000Z\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-02T00:00:00Z", 24 * 7)).expect("expands");
        assert_eq!(ids(&events).len(), 2, "Oct 2 and Oct 3, never Oct 4");
    }

    #[test]
    fn an_unsupported_rule_degrades_to_dtstart() {
        let raw = "BEGIN:VEVENT\nUID:odd@x\nDTSTART:20261001T080000Z\n\
                   RRULE:FREQ=MONTHLY;BYDAY=1MO\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-01T00:00:00Z", 24)).expect("expands");
        assert_eq!(ids(&events).len(), 1);
        assert_eq!(events[0].start.day(), 1);
    }

    #[test]
    fn an_override_replaces_its_occurrence_and_a_cancelled_one_removes_it() {
        // Daily at 09:00; an override moves Oct 2's to 15:00, a
        // cancelled one removes Oct 3's.
        let raw = "BEGIN:VEVENT\nUID:week@x\nDTSTART:20261001T090000Z\n\
                   RRULE:FREQ=DAILY;COUNT=5\nEND:VEVENT\n\
                   BEGIN:VEVENT\nUID:week@x\nRECURRENCE-ID:20261002T090000Z\n\
                   DTSTART:20261002T150000Z\nSUMMARY:moved\nEND:VEVENT\n\
                   BEGIN:VEVENT\nUID:week@x\nRECURRENCE-ID:20261003T090000Z\n\
                   DTSTART:20261003T090000Z\nSTATUS:CANCELLED\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-01T00:00:00Z", 24 * 7)).expect("expands");
        let starts: Vec<i64> = events
            .iter()
            .map(|event| event.start.unix_timestamp())
            .collect();
        assert_eq!(starts.len(), 4, "5 occurrences, one moved, one cancelled");
        assert!(
            !starts.contains(
                &OffsetDateTime::parse("2026-10-02T09:00:00Z", &Rfc3339)
                    .expect("parses")
                    .unix_timestamp()
            )
        );
        assert!(
            starts.contains(
                &OffsetDateTime::parse("2026-10-02T15:00:00Z", &Rfc3339)
                    .expect("parses")
                    .unix_timestamp()
            )
        );
    }

    #[test]
    fn an_override_moving_an_occurrence_into_the_window_does_not_double() {
        let raw = "BEGIN:VEVENT\nUID:move@x\nDTSTART:20261001T090000Z\n\
                   RRULE:FREQ=DAILY;COUNT=3\nEND:VEVENT\n\
                   BEGIN:VEVENT\nUID:move@x\nRECURRENCE-ID:20261001T090000Z\n\
                   DTSTART:20261020T150000Z\nEND:VEVENT\n";
        // The window sees only the moved instance; the Oct 1 original
        // it replaced is outside and must not resurrect.
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        let moved = OffsetDateTime::parse("2026-10-20T15:00:00Z", &Rfc3339).expect("parses");
        assert_eq!(
            ids(&events),
            vec![format!("ics:move@x@{}", moved.unix_timestamp())]
        );
    }

    #[test]
    fn exdate_drops_occurrences() {
        let raw = "BEGIN:VEVENT\nUID:skip@x\nDTSTART:20261001T090000Z\n\
                   RRULE:FREQ=DAILY;COUNT=5\nEXDATE:20261002T090000Z\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-01T00:00:00Z", 24 * 7)).expect("expands");
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn cancelled_masters_and_gtodos_are_ignored() {
        let raw = "BEGIN:VTODO\nUID:task@x\nDTSTART:20261020T140000Z\nEND:VTODO\n\
                   BEGIN:VEVENT\nUID:gone@x\nDTSTART:20261020T140000Z\nSTATUS:CANCELLED\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert!(events.is_empty());
    }

    #[test]
    fn an_event_inside_a_valarm_is_not_mistaken_for_a_property() {
        let raw = "BEGIN:VEVENT\nUID:alarm@x\nDTSTART:20261020T140000Z\nSUMMARY:real\n\
                   BEGIN:VALARM\nUID:fake@x\nDTSTART:20261020T134500Z\nSUMMARY:alarm\nEND:VALARM\n\
                   END:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "real");
    }

    #[test]
    fn a_row_without_uid_or_dtstart_fails_the_feed_loudly() {
        let no_uid = "BEGIN:VEVENT\nDTSTART:20261020T140000Z\nEND:VEVENT\n";
        let err = expand(no_uid, &window("2026-10-20T00:00:00Z", 24)).expect_err("loud");
        assert!(err.to_string().contains("UID"), "got: {err}");

        let no_start = "BEGIN:VEVENT\nUID:a@x\nSUMMARY:when?\nEND:VEVENT\n";
        let err = expand(no_start, &window("2026-10-20T00:00:00Z", 24)).expect_err("loud");
        assert!(err.to_string().contains("DTSTART"), "got: {err}");
    }

    #[test]
    fn an_all_day_row_starts_at_midnight_and_says_so() {
        let raw = "BEGIN:VEVENT\nUID:day@x\nDTSTART;VALUE=DATE:20261020\nSUMMARY:offsite\n\
                   DTEND;VALUE=DATE:20261021\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].duration(), time::Duration::days(1));
        assert!(events[0].start.to_string().contains("00:00"));
    }

    #[test]
    fn a_tzid_parameter_rides_the_raw_row() {
        let raw = "BEGIN:VEVENT\nUID:tz@x\nDTSTART;TZID=Europe/Paris:20261020T140000\nEND:VEVENT\n";
        let events = expand(raw, &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert_eq!(
            events[0].raw.get("DTSTART;TZID=Europe/Paris"),
            Some(&Value::String("20261020T140000".to_owned()))
        );
    }
}
