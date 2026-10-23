//! The contract's own gate: the suite passes against the mock router.
//! The daemon's real router runs the same suite in its test tree —
//! same results is the definition of implementing the contract.

use rhema_tools_contract::{CallRequest, MockClaw, conformance};

#[tokio::test]
async fn the_suite_passes_against_the_mock_router() {
    let mock = MockClaw::new(Some("human-token"), Some("agent-token"));
    let result =
        conformance(|request: CallRequest| async { Ok(mock.call(request).await.unwrap()) }).await;
    assert_eq!(result, Ok(()), "the mock router speaks the contract");
}

#[tokio::test]
async fn an_open_mock_fails_the_suite() {
    // No tokens: the agent-token guarantees the suite exists to check
    // are gone. The suite must say so, not sail through.
    let mock = MockClaw::new(None, None);
    let result =
        conformance(|request: CallRequest| async { Ok(mock.call(request).await.unwrap()) }).await;
    assert!(result.is_err(), "an open surface cannot pass the suite");
}

#[tokio::test]
async fn the_agent_allowlist_reads_the_documented_routes() {
    assert!(rhema_tools_contract::agent_allowed("GET", "/v1/watches"));
    assert!(rhema_tools_contract::agent_allowed(
        "DELETE",
        "/v1/watches/github/acme/widgets"
    ));
    assert!(rhema_tools_contract::agent_allowed(
        "POST",
        "/v1/notifications"
    ));
    assert!(rhema_tools_contract::agent_allowed(
        "DELETE",
        "/v1/tasks/12"
    ));
    assert!(!rhema_tools_contract::agent_allowed("POST", "/v1/ingest"));
    assert!(!rhema_tools_contract::agent_allowed(
        "POST",
        "/v1/consents/c-1"
    ));
    assert!(!rhema_tools_contract::agent_allowed("GET", "/v1/consents"));
    assert!(!rhema_tools_contract::agent_allowed(
        "DELETE",
        "/v1/phones/p1"
    ));
    assert!(
        !rhema_tools_contract::agent_allowed("GET", "/v1/watches/github/acme/widgets"),
        "only DELETE reaches subpaths of /v1/watches"
    );
}
