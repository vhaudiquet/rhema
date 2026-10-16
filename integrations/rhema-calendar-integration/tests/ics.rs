//! Offline integration tests: the ICS backend against an in-process
//! wiremock server. Zero network.

use rhema_calendar_integration::{Calendar, CalendarError, IcsCalendar, Window};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FEED_BODY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VEVENT\r\nUID:meet-1@x\r\nDTSTART:20261020T140000Z\r\nDTEND:20261020T150000Z\r\n\
SUMMARY:kickoff\\, phase one\r\nLOCATION:paris\r\n\
ORGANIZER:mailto:boss@example.net\r\nATTENDEE:mailto:alice@example.net\r\n\
URL:https://meet.example.net/1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn window() -> Window {
    let from = OffsetDateTime::parse("2026-10-20T00:00:00Z", &Rfc3339).expect("parses");
    Window::new(from, from + time::Duration::hours(24))
}

#[tokio::test]
async fn a_poll_is_one_get_and_normalizes_the_feed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/remote.php/dav/calendars/personal.ics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FEED_BODY))
        .mount(&server)
        .await;

    let feed = IcsCalendar::new(format!(
        "{}/remote.php/dav/calendars/personal.ics",
        server.uri()
    ));
    let events = feed.events(&window()).await.expect("polls");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.title, "kickoff, phase one");
    assert_eq!(event.location.as_deref(), Some("paris"));
    assert_eq!(event.organizer.as_deref(), Some("boss@example.net"));
    assert_eq!(event.attendees, vec!["alice@example.net".to_owned()]);
    assert_eq!(event.url.as_deref(), Some("https://meet.example.net/1"));
    assert_eq!(
        event.start,
        OffsetDateTime::parse("2026-10-20T14:00:00Z", &Rfc3339).expect("parses")
    );
}

#[tokio::test]
async fn a_token_rides_the_authorization_header_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("Authorization", "Bearer sekrit"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FEED_BODY))
        .mount(&server)
        .await;

    let feed = IcsCalendar::new(server.uri()).with_token("sekrit");
    feed.events(&window()).await.expect("polls");
}

#[tokio::test]
async fn a_server_error_is_named_with_a_bounded_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("maintenance"))
        .mount(&server)
        .await;

    let feed = IcsCalendar::new(server.uri());
    let err = feed.events(&window()).await.expect_err("loud");
    assert!(
        matches!(err, CalendarError::Api { status: 503, .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("maintenance"), "got: {err}");
}
