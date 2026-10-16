//! The spec §8.5 worked example, end to end: an assignment dispatch, live
//! comments landing in the running handler's inbox, a clean release to
//! idle — and the variant where a comment slips in between the last poll
//! and release, forcing a re-acquire of the same session.

mod common;

use common::{DEBOUNCE, event, register_github, setup, the_key};
use rhema_core::{Message, Role};
use rhema_rsd::{IngestResult, KeyState, Outcome};

#[test]
fn assignment_live_comments_and_clean_release_end_at_idle() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();

    // 1. The assignment comment arrives at an idle key: recorded, debounce
    //    opens, nothing dispatches yet.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e1")).unwrap(),
        IngestResult::DebouncePending
    );
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Idle);

    clock.advance(DEBOUNCE);
    let admitted = dispatcher.admit().unwrap();
    assert_eq!(admitted, vec![key.clone()]);
    let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    assert_eq!(lease.revision, 1);
    assert!(session.core.history.is_empty());
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    // The handler consumes its triggering assignment at the first poll.
    let items = dispatcher.poll_inbox(&lease).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].event.id, "e1");

    // The handler plans, mints an artifact, checkpoints.
    session.plan = Some(serde_json::json!(["read issue", "implement"]));
    session.artifact_ref = Some("worktrees/issue-142".into());
    session
        .core
        .history
        .push(Message::text(Role::User, "implement the fix for #142"));
    dispatcher.checkpoint(&lease, session.clone()).unwrap();

    // 2. A comment arrives mid-implementation: appended straight to the
    //    live inbox, no state change, no debounce.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e2")).unwrap(),
        IngestResult::DeliveredLive
    );
    assert_eq!(dispatcher.status(&key).unwrap().state, KeyState::Leased);
    let items = dispatcher.poll_inbox(&lease).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].event.id, "e2");

    // The handler answers from what it already knows and checkpoints.
    session.core.history.push(Message::text(
        Role::User,
        "inbox: no PR yet? what is happening?",
    ));
    session.core.history.push(Message::text(
        Role::Assistant,
        "still in progress, ETA ~20 min",
    ));
    dispatcher.checkpoint(&lease, session.clone()).unwrap();

    // 3. A fix suggestion arrives; same live path.
    assert_eq!(
        dispatcher.ingest(event(key.as_str(), "e3")).unwrap(),
        IngestResult::DeliveredLive
    );
    let items = dispatcher.poll_inbox(&lease).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].event.id, "e3");
    session
        .core
        .history
        .push(Message::text(Role::User, "inbox: watch out for bug X"));
    dispatcher.checkpoint(&lease, session.clone()).unwrap();

    // 4. The handler finishes with an empty inbox: the key goes idle.
    dispatcher.release(&lease, Outcome::Handled).unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Idle);
    assert_eq!(status.inbox_depth, 0);
    assert_eq!(status.last_outcome, Some(Outcome::Handled));
}

#[test]
fn a_comment_between_last_poll_and_release_reacquires_the_same_session() {
    let (dispatcher, clock) = setup();
    register_github(&dispatcher);
    let key = the_key();

    dispatcher.ingest(event(key.as_str(), "e1")).unwrap();
    clock.advance(DEBOUNCE);
    dispatcher.admit().unwrap();
    let (lease, mut session) = dispatcher.acquire(key.clone(), "handler-1").unwrap();
    assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);
    session
        .core
        .history
        .push(Message::text(Role::User, "implement"));
    session.artifact_ref = Some("worktrees/issue-142".into());
    dispatcher.checkpoint(&lease, session.clone()).unwrap();

    // Everything delivered is consumed...
    dispatcher.ingest(event(key.as_str(), "e2")).unwrap();
    assert_eq!(dispatcher.poll_inbox(&lease).unwrap().len(), 1);

    // ...and a third comment lands in the gap between the last poll and
    // release: the non-empty inbox must trigger re-acquire, not idle.
    dispatcher.ingest(event(key.as_str(), "e3")).unwrap();
    dispatcher.release(&lease, Outcome::Handled).unwrap();
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(status.state, KeyState::Reacquire);
    assert_eq!(status.inbox_depth, 1);
    assert_eq!(status.revision, 1);

    // The re-dispatch resumes the very same session: same history, same
    // artifact, one revision higher.
    clock.advance(DEBOUNCE);
    assert_eq!(dispatcher.admit().unwrap(), vec![key.clone()]);
    let (lease2, session2) = dispatcher.acquire(key.clone(), "handler-2").unwrap();
    assert_eq!(lease2.revision, 2);
    assert_ne!(lease2.id, lease.id);
    assert_eq!(session2, session);
    // The unpolled comment is still there for the new handler.
    assert_eq!(dispatcher.poll_inbox(&lease2).unwrap().len(), 1);
}
