//! The client end to end over real HTTP: a small axum server shaped
//! exactly like the daemon's contract routes (bearer + contract
//! header checked, the same JSON), the binary's logic driven through
//! [`rhema_tools_common::Client`].

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use rhema_tools_common::{Client, Config, ToolError};
use rhema_tools_contract::NotificationPush;
use serde_json::{Value, json};

/// What the fake daemon recorded, for the test to assert on.
#[derive(Default, Clone)]
struct Recorded(Arc<std::sync::Mutex<Vec<Value>>>);

/// Spawns the contract-shaped fake daemon on an ephemeral port; the
/// address comes back.
async fn serve(recorded: Recorded) -> String {
    let app = axum::Router::new()
        .route(
            "/v1/notifications",
            get(|| async { Json(json!({"notifications": []})) }).post(push),
        )
        .with_state(recorded);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serves");
    });
    format!("http://{address}")
}

/// The push handler: the auth and contract checks a real route does,
/// then the row back.
async fn push(
    State(recorded): State<Recorded>,
    headers: HeaderMap,
    Json(body): Json<NotificationPush>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer agent-token") {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        ));
    }
    if headers
        .get("x-rhema-contract")
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "contract version mismatch"})),
        ));
    }
    recorded
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(json!({"title": body.title, "device": body.device}));
    Ok(Json(json!({
        "id": 7, "device": body.device.unwrap_or_else(|| "phone".into()),
        "title": body.title, "body": body.body,
        "created_at": 0, "delivered_at": null
    })))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_rides_the_contract_and_prints_the_row() {
    let recorded = Recorded::default();
    let base_url = serve(recorded.clone()).await;
    let client = Client::new(Config {
        base_url,
        token: Some("agent-token".into()),
    });
    let row = client
        .call(
            "POST",
            rhema_tools_contract::NOTIFICATIONS,
            Some(&NotificationPush {
                title: "CI is red".into(),
                body: "main failed".into(),
                device: None,
            }),
        )
        .expect("the push lands");
    assert_eq!(row["id"], 7);
    assert_eq!(row["device"], "phone");
    let pushes = recorded
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["title"], "CI is red");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_maps_to_denied() {
    let base_url = serve(Recorded::default()).await;
    let client = Client::new(Config {
        base_url,
        token: Some("the-wrong-token".into()),
    });
    let error = client
        .call(
            "POST",
            rhema_tools_contract::NOTIFICATIONS,
            Some(&NotificationPush {
                title: "x".into(),
                body: "y".into(),
                device: None,
            }),
        )
        .expect_err("the wrong token is refused");
    assert!(matches!(error, ToolError::Denied(_)));
    assert_eq!(error.exit_code(), 3);
}
