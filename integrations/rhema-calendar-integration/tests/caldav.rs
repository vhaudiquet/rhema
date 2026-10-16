//! Offline integration tests: the CalDAV backend against an in-process
//! wiremock server. Zero network.

use rhema_calendar_integration::{CaldavCalendar, Calendar, CalendarError, Window};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn window() -> Window {
    // Oct 1 00:00 to Oct 3 00:00: wide enough that the recurring
    // master's second day lands inside, and the `time-range` bound in
    // the request reads `end=20261003T000000Z`.
    let from = OffsetDateTime::parse("2026-10-01T00:00:00Z", &Rfc3339).expect("parses");
    Window::new(from, from + time::Duration::hours(48))
}

/// The 207 answer: one single event, one recurring master with a moved
/// override. The iCalendar objects carry `&#13;&#10;` where CRLF
/// belongs, because that is how servers spell an ICS inside XML, and
/// an unexpanded reference would fold the object into one unreadable
/// line.
const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/calendars/me/home/kickoff.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"1"</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR&#13;&#10;VERSION:2.0&#13;&#10;BEGIN:VEVENT&#13;&#10;UID:kickoff@x&#13;&#10;DTSTART:20261001T140000Z&#13;&#10;DTEND:20261001T150000Z&#13;&#10;SUMMARY:kickoff&#13;&#10;END:VEVENT&#13;&#10;END:VCALENDAR</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/calendars/me/home/standup.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"2"</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR&#13;&#10;BEGIN:VEVENT&#13;&#10;UID:standup@x&#13;&#10;DTSTART:20261001T090000Z&#13;&#10;RRULE:FREQ=DAILY;COUNT=3&#13;&#10;SUMMARY:standup&#13;&#10;END:VEVENT&#13;&#10;BEGIN:VEVENT&#13;&#10;UID:standup@x&#13;&#10;RECURRENCE-ID:20261001T090000Z&#13;&#10;DTSTART:20261001T160000Z&#13;&#10;SUMMARY:standup moved&#13;&#10;END:VEVENT&#13;&#10;END:VCALENDAR</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

fn calendar(server: &MockServer) -> CaldavCalendar {
    CaldavCalendar::new(format!("{}/calendars/me/home/", server.uri())).with_basic("me", "app-pass")
}

fn starts(events: &[rhema_calendar_integration::CalendarEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.start.format(&Rfc3339).expect("formats"))
        .collect()
}

#[tokio::test]
async fn a_poll_is_one_report_and_expands_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("REPORT"))
        .and(path("/calendars/me/home"))
        .and(header("Depth", "1"))
        .and(body_string_contains(
            r#"<C:time-range start="20261001T000000Z" end="20261003T000000Z"/>"#,
        ))
        .respond_with(ResponseTemplate::new(207).set_body_string(MULTISTATUS))
        .mount(&server)
        .await;

    let events = calendar(&server).events(&window()).await.expect("polls");
    // The master's Oct 1 occurrence is suppressed by the override that
    // moved it to 16:00; the Oct 2 occurrence is in the window, and
    // the Oct 3 one (COUNT=3) is past its end.
    assert_eq!(events.len(), 3, "{:?}", starts(&events));
    assert_eq!(events[0].title, "kickoff");
    assert_eq!(events[1].title, "standup moved");
    assert_eq!(events[2].title, "standup");
    for event in &events {
        assert!(
            event.id.starts_with("caldav:"),
            "occurrence ids spell the adapter: {}",
            event.id
        );
    }
    assert!(starts(&events)[0].contains("14:00"));
    assert!(starts(&events)[1].contains("16:00"));
    assert!(starts(&events)[2].contains("2026-10-02T09:00"));
}

#[tokio::test]
async fn basic_credentials_ride_the_authorization_header_only() {
    let server = MockServer::start().await;
    Mock::given(method("REPORT"))
        .and(header("Authorization", "Basic bWU6YXBwLXBhc3M="))
        .respond_with(ResponseTemplate::new(207).set_body_string(MULTISTATUS))
        .mount(&server)
        .await;

    calendar(&server).events(&window()).await.expect("polls");
}

#[tokio::test]
async fn a_bearer_token_is_the_other_auth_spelling() {
    let server = MockServer::start().await;
    Mock::given(method("REPORT"))
        .and(header("Authorization", "Bearer sekrit"))
        .respond_with(ResponseTemplate::new(207).set_body_string(MULTISTATUS))
        .mount(&server)
        .await;

    let calendar = CaldavCalendar::new(server.uri()).with_token("sekrit");
    calendar.events(&window()).await.expect("polls");
}

#[tokio::test]
async fn a_server_error_is_named() {
    let server = MockServer::start().await;
    Mock::given(method("REPORT"))
        .respond_with(ResponseTemplate::new(503).set_body_string("maintenance"))
        .mount(&server)
        .await;

    let err = calendar(&server).events(&window()).await.expect_err("loud");
    assert!(
        matches!(err, CalendarError::Api { status: 503, .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn a_non_xml_answer_is_a_loud_parse_error() {
    let server = MockServer::start().await;
    // An HTML login page can arrive with a 200; almost none of it is
    // well-formed XML, and a shape break must surface, not read as a
    // quietly empty calendar.
    Mock::given(method("REPORT"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<D:multistatus></D:response>"))
        .mount(&server)
        .await;

    let err = calendar(&server).events(&window()).await.expect_err("loud");
    assert!(matches!(err, CalendarError::Parse(_)), "got: {err:?}");
}
