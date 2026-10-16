//! Serde round-trips: the wrapped session (including crate-1 history and
//! driver state) and the full key record must survive a checkpoint
//! verbatim — the property the whole persistence story rests on.

mod common;

use common::{DEBOUNCE, event, register_github, setup, the_key};
use rhema_core::{Content, Message, Role, ToolCall};
use rhema_rsd::{KeyState, Outcome};
use serde_json::json;

#[test]
fn a_dispatcher_session_round_trips_with_its_wrapped_core_state() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();
    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();

    // A session mid-work: user turn, assistant turn with a tool call, tool
    // result, opaque driver state, plan, artifact.
    session
        .core
        .history
        .push(Message::text(Role::User, "fix it"));
    session.core.history.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Text("running the check".into()),
            Content::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "cargo_test".into(),
                args: json!({ "package": "rhema-rsd" }),
                provider_data: json!({ "signature": "opaque" }),
            }),
        ],
    });
    session.core.history.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(rhema_core::ToolResult::success(
            "call_1", "ok",
        ))],
    });
    session.core.driver_state = json!({ "attempt": 3 });
    session.plan = Some(json!(["reproduce", "fix", "verify"]));
    session.artifact_ref = Some("worktrees/issue-142".into());
    // Checkpointing persists the session verbatim; the round-trip through
    // JSON must reproduce it exactly, driver state and tool blocks
    // included.
    dispatcher.checkpoint(&lease, session.clone()).unwrap();

    let back: rhema_rsd::Session = serde_json::from_value(json!(&session)).unwrap();
    assert_eq!(back, session);
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.revision, 1);
    assert_eq!(status.state, KeyState::Leased);
}

#[test]
fn a_full_key_record_round_trips_through_serde() {
    let record = rhema_rsd::KeyRecord {
        key: the_key(),
        state: KeyState::Reacquire,
        revision: 4,
        lease: Some(rhema_rsd::LeaseRecord {
            id: rhema_rsd::LeaseId::from("lease-3"),
            holder: "handler-1".into(),
            acquired_at: 1_100,
            expires_at: 1_100 + 10_000,
        }),
        inbox: vec![rhema_rsd::InboxItem {
            event: event(the_key().as_str(), "e2"),
            delivered_at: 1_500,
        }],
        source: "github".into(),
        session: Some(rhema_rsd::Session::fresh(the_key(), 1_100)),
        debounce_deadline: Some(2_000),
        admitted_at: None,
        last_outcome: Some(Outcome::Superseded),
        updated_at: 1_999,
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: rhema_rsd::KeyRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}
