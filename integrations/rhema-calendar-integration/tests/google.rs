//! Offline integration tests: the Google Calendar backend against an
//! in-process wiremock server. Zero network.

use rhema_calendar_integration::{Calendar, CalendarError, GoogleCalendar, Window};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn window() -> Window {
    let from = OffsetDateTime::parse("2026-10-20T00:00:00Z", &Rfc3339).expect("parses");
    Window::new(from, from + time::Duration::hours(24))
}

/// A timed row; `patch` overrides any field.
fn row(id: &str, patch: Value) -> Value {
    let mut base = json!({
        "id": id,
        "status": "confirmed",
        "summary": format!("event {id}"),
        "start": { "dateTime": "2026-10-20T14:00:00+02:00" },
        "end": { "dateTime": "2026-10-20T15:00:00+02:00" },
        "htmlLink": format!("https://calendar.google.com/x/{id}")
    });
    if let (Value::Object(base), Value::Object(patch)) = (&mut base, patch) {
        for (key, value) in patch {
            base.insert(key, value);
        }
    }
    base
}

#[tokio::test]
async fn a_poll_asks_for_expanded_instances_ordered_by_start() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/primary/events"))
        .and(query_param("singleEvents", "true"))
        .and(query_param("orderBy", "startTime"))
        .and(query_param("timeMin", "2026-10-20T00:00:00Z"))
        .and(query_param("timeMax", "2026-10-21T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [
            row("a", json!({})),
        ]})))
        .mount(&server)
        .await;

    let calendar = GoogleCalendar::new("tok", "primary").with_base_url(server.uri());
    let events = calendar.events(&window()).await.expect("polls");
    assert_eq!(events.len(), 1);
    // An offset start is an instant: the id pins the UTC second.
    let at = OffsetDateTime::parse("2026-10-20T12:00:00Z", &Rfc3339).expect("parses");
    assert_eq!(events[0].id, format!("google:a@{}", at.unix_timestamp()));
}

#[tokio::test]
async fn pages_are_followed_until_the_cursor_ends() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("pageToken", "cursor-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [
            row("last", json!({}))
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [row("first", json!({}))],
            "nextPageToken": "cursor-2"
        })))
        .mount(&server)
        .await;

    let calendar = GoogleCalendar::new("tok", "primary").with_base_url(server.uri());
    let events = calendar.events(&window()).await.expect("polls");
    let ids: Vec<&str> = events
        .iter()
        .map(|event| event.id.split('@').next().unwrap_or(""))
        .collect();
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn an_api_key_rides_the_key_parameter_and_a_token_the_header() {
    let server = MockServer::start().await;
    // The key mock is mounted first and wins the requests it matches;
    // everything else (the keyless bearer client below) falls through
    // to the refusal.
    Mock::given(method("GET"))
        .and(query_param("key", "pub-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [
            row("public", json!({}))
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;

    let calendar = GoogleCalendar::with_api_key("pub-key", "primary").with_base_url(server.uri());
    assert_eq!(calendar.events(&window()).await.expect("polls").len(), 1);

    // A client built with a token never sends `key`, so it lands on
    // the refusal: the credential kind decides the wire shape.
    let calendar = GoogleCalendar::new("tok", "primary").with_base_url(server.uri());
    let err = calendar
        .events(&window())
        .await
        .expect_err("the keyless request is refused");
    assert!(
        matches!(err, CalendarError::Api { status: 400, .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn cancelled_rows_do_not_become_reminders() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [
            row("kept", json!({})),
            row("gone", json!({ "status": "cancelled" }))
        ]})))
        .mount(&server)
        .await;

    let calendar = GoogleCalendar::new("tok", "primary").with_base_url(server.uri());
    let events = calendar.events(&window()).await.expect("polls");
    assert_eq!(events.len(), 1);
    assert!(events[0].id.contains("kept"));
}

#[tokio::test]
async fn a_server_error_is_named() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("quota"))
        .mount(&server)
        .await;

    let calendar = GoogleCalendar::new("tok", "primary").with_base_url(server.uri());
    let err = calendar.events(&window()).await.expect_err("loud");
    assert!(
        matches!(err, CalendarError::Api { status: 403, .. }),
        "got: {err:?}"
    );
}
