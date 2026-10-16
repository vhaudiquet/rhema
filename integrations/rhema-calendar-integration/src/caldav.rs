//! The CalDAV adapter: one `REPORT` per poll against a calendar
//! collection, the occurrences read out of the iCalendar objects the
//! server answers with.
//!
//! Wire choices and the constraints they pin:
//! - The request is a `CALDAV:calendar-query` `REPORT` with a
//!   `time-range` on `VEVENT` and `Depth: 1`. The URL is the calendar
//!   collection's own, a copy-paste from the server UI; principal and
//!   home-set discovery are deliberately absent, because a hand-pasted
//!   URL is one config line and discovery is a protocol zoo.
//! - The answer is a 207 multistatus whose `calendar-data` values are
//!   whole iCalendar objects. Servers return recurring masters plus
//!   overrides for the range, so the expansion in [`crate::ics`] does
//!   the client-side work and this module only fetches and parses.
//!   Occurrence ids carry the `caldav` prefix, so a calendar moved
//!   between adapters changes its dedup ids: feed URLs and DAV
//!   collections are different identities, not spellings of one.
//! - Auth is Basic (username plus password, the self-hosted norm), a
//!   bearer token, or anonymous, whichever the server wants.
//!   Credentials ride the `Authorization` header only, never a URL or
//!   a log line.
//! - A `time-range` `REPORT` filters server-side by overlap, so the
//!   adapter still clips client-side to the window's own contract:
//!   starts within `[from, to)`.
//! - Non-event objects in the collection (a `VTODO` stored beside the
//!   events) expand to nothing. A `VEVENT` without `UID` or `DTSTART`
//!   fails the poll loudly, as in the ics adapter: a shape break must
//!   surface, not shrink the calendar.

use std::time::Duration;

use async_trait::async_trait;
use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::Event as XmlEvent;
use time::OffsetDateTime;

use crate::calendar::{Calendar, finish};
use crate::error::{CalendarError, ensure_success};
use crate::event::{CalendarEvent, Window};
use crate::ics::expand_as;

/// The production spelling of nothing in particular: CalDAV has no
/// vendor base to default to, the calendar's URL is configured whole.
pub const USER_AGENT: &str = concat!("rhema-calendar-integration/", env!("CARGO_PKG_VERSION"));

/// How the adapter authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    /// No credentials; servers that publish openly.
    Anonymous,
    /// Username plus password, the self-hosted norm.
    Basic { user: String, password: String },
    /// A bearer token, for servers that issue them.
    Bearer(String),
}

/// CalDAV backend: a collection URL, an auth choice, one reqwest
/// client.
pub struct CaldavCalendar {
    url: String,
    auth: Auth,
    http: reqwest::Client,
    report: reqwest::Method,
}

impl CaldavCalendar {
    /// A calendar collection at `url` (`https://` or `http://`; a
    /// self-hosted collection on the LAN is a legitimate http
    /// citizen). Anonymous until told otherwise.
    pub fn new(url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // One connect attempt: a dead server fails fast instead of
            // stalling the poll loop.
            .connect_timeout(Duration::from_secs(10))
            // Every DAV call is a bounded request/response, so one
            // overall timeout per request is the right guard.
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self {
            url: url.into().trim_end_matches('/').to_owned(),
            auth: Auth::Anonymous,
            http,
            // REPORT is an HTTP token, so this construction cannot
            // fail; reqwest has no const constructor for it.
            report: reqwest::Method::from_bytes(b"REPORT")
                .expect("REPORT is a valid HTTP method token"),
        }
    }

    /// Authenticates with Basic credentials.
    pub fn with_basic(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Auth::Basic {
            user: user.into(),
            password: password.into(),
        };
        self
    }

    /// Authenticates with a bearer token.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.auth = Auth::Bearer(token.into());
        self
    }
}

#[async_trait]
impl Calendar for CaldavCalendar {
    async fn events(&self, window: &Window) -> Result<Vec<CalendarEvent>, CalendarError> {
        let mut request = self
            .http
            .request(self.report.clone(), &self.url)
            .header("Depth", "1")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .header(reqwest::header::ACCEPT, "application/xml")
            .body(report_body(window));
        match &self.auth {
            Auth::Anonymous => {}
            Auth::Basic { user, password } => {
                request = request.basic_auth(user, Some(password));
            }
            Auth::Bearer(token) => {
                request = request.bearer_auth(token);
            }
        }
        let response = request.send().await.map_err(CalendarError::network)?;
        let body = ensure_success(response)
            .await?
            .text()
            .await
            .map_err(CalendarError::network)?;
        let mut events = Vec::new();
        for object in parse_multistatus(&body)? {
            events.extend(expand_as(&object, "caldav", window)?);
        }
        Ok(finish(events, window))
    }
}

/// The `calendar-query` body: the collection's VEVENTs, bounded by the
/// window's `time-range`. The bounds are RFC 4791's own spelling:
/// compact, UTC, `Z`.
fn report_body(window: &Window) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start}" end="{end}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#,
        start = compact_z(window.from),
        end = compact_z(window.to),
    )
}

/// An instant as a CalDAV `time-range` bound: `YYYYMMDDTHHMMSSZ` in
/// UTC, whatever offset the instant carried.
fn compact_z(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// An element's local name: CalDAV's prefixes (`D:`, `C:`) are
/// conventions, not guarantees, and the answer's meaning lives in the
/// namespace URIs the element binds. Matching local names inside a
/// `prop` stat is the pragmatic read every DAV client ships.
fn local_name(name: quick_xml::name::QName<'_>) -> &str {
    // The reader was built from a &str, so every borrowed name is
    // UTF-8 already; a decoding failure cannot say anything truer than
    // "not the element we were looking for".
    std::str::from_utf8(name.local_name().into_inner()).unwrap_or("")
}

/// Pulls every `calendar-data` value out of a 207 multistatus, in
/// document order. Whitespace-only values are dropped; anything else
/// the server said (hrefs, etags, statuses) is beneath the vocabulary.
fn parse_multistatus(xml: &str) -> Result<Vec<String>, CalendarError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut datas = Vec::new();
    let mut capturing = false;
    let mut current = String::new();
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(element)) => {
                if local_name(element.name()) == "calendar-data" {
                    capturing = true;
                    current.clear();
                }
            }
            Ok(XmlEvent::Text(text)) if capturing => {
                // Escaped fragments ride Text; references ride their
                // own event (below). A server writes `&#13;&#10;`
                // where the iCalendar needs CRLF, and an unexpanded
                // reference would fold the whole object into one line
                // and read as quietly empty.
                let decoded = text
                    .xml10_content()
                    .map_err(|err| CalendarError::Parse(format!("multistatus text: {err}")))?;
                let decoded = unescape(&decoded)
                    .map_err(|err| CalendarError::Parse(format!("multistatus text: {err}")))?;
                current.push_str(&decoded);
            }
            Ok(XmlEvent::GeneralRef(reference)) if capturing => {
                match reference.resolve_char_ref() {
                    // `&#13;` and friends: one character.
                    Ok(Some(ch)) => current.push(ch),
                    // A named entity (`&amp;`): the escape table's.
                    Ok(None) => {
                        let raw = reference.xml10_content().map_err(|err| {
                            CalendarError::Parse(format!("multistatus text: {err}"))
                        })?;
                        let entity = format!("&{raw};");
                        let decoded = unescape(&entity).map_err(|err| {
                            CalendarError::Parse(format!("multistatus text: {err}"))
                        })?;
                        current.push_str(&decoded);
                    }
                    Err(err) => {
                        return Err(CalendarError::Parse(format!("multistatus text: {err}")));
                    }
                }
            }
            Ok(XmlEvent::CData(text)) if capturing => {
                current.push_str(&String::from_utf8_lossy(text.as_ref()));
            }
            Ok(XmlEvent::End(element)) => {
                if capturing && local_name(element.name()) == "calendar-data" {
                    capturing = false;
                    if !current.trim().is_empty() {
                        datas.push(std::mem::take(&mut current));
                    }
                }
            }
            Ok(XmlEvent::Eof) => break,
            Ok(_) => {}
            Err(err) => return Err(CalendarError::Parse(format!("multistatus: {err}"))),
        }
    }
    Ok(datas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::format_description::well_known::Rfc3339;

    fn window(start: &str, hours: i64) -> Window {
        let from = OffsetDateTime::parse(start, &time::format_description::well_known::Rfc3339)
            .expect("parses");
        Window::new(from, from + time::Duration::hours(hours))
    }

    #[test]
    fn time_range_bounds_are_compact_utc() {
        // An offset instant normalizes: +02:00 noon is 10:00Z.
        let at = OffsetDateTime::parse("2026-10-20T12:00:00+02:00", &Rfc3339).expect("parses");
        assert_eq!(compact_z(at), "20261020T100000Z");
        assert_eq!(
            compact_z(window("2026-10-20T00:00:00Z", 24).to),
            "20261021T000000Z"
        );
    }

    #[test]
    fn the_report_names_the_window_in_caldavs_own_spelling() {
        let body = report_body(&window("2026-10-20T00:00:00Z", 24));
        assert!(
            body.contains(r#"<C:time-range start="20261020T000000Z" end="20261021T000000Z"/>"#)
        );
        assert!(body.contains(r#"<C:comp-filter name="VEVENT">"#));
        assert!(body.contains("calendar-query"));
    }

    #[test]
    fn multistatus_yields_every_calendar_data_in_order() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/c/me/home/a.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"1"</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR
FIRST:a
END:VCALENDAR</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/c/me/home/b.ics</D:href>
    <D:propstat>
      <D:prop>
        <C:calendar-data>BEGIN:VCALENDAR
FIRST:b
END:VCALENDAR</C:calendar-data>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let datas = parse_multistatus(xml).expect("parses");
        assert_eq!(datas.len(), 2);
        assert!(datas[0].contains("FIRST:a"));
        assert!(datas[1].contains("FIRST:b"));
    }

    #[test]
    fn empty_and_missing_calendar_data_contribute_nothing() {
        let xml = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:propstat>
      <D:prop>
        <C:calendar-data/>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:propstat>
      <D:prop>
        <D:getetag>"2"</D:getetag>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        assert!(parse_multistatus(xml).expect("parses").is_empty());
    }

    #[test]
    fn garbage_xml_is_a_loud_parse_error() {
        let err = parse_multistatus("<not closed").expect_err("loud");
        assert!(matches!(err, CalendarError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn a_vtodo_only_collection_expands_to_nothing() {
        // expand_as is the shared expansion; the CalDAV spelling of it
        // must treat a non-event object as quiet, not broken.
        let object = "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:t@x\nDTSTART:20261020T140000Z\n\
                      END:VTODO\nEND:VCALENDAR\n";
        let events =
            expand_as(object, "caldav", &window("2026-10-20T00:00:00Z", 24)).expect("expands");
        assert!(events.is_empty());
    }
}
