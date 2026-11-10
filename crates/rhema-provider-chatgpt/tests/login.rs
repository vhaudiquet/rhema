//! The browser OAuth flow against a wiremock token endpoint, with the
//! test playing the browser.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use rhema_provider::{InteractiveLogin, LoginSink, LoginUpdate};
use rhema_provider_chatgpt::{ChatGptCredential, ChatGptLogin};
use tokio::sync::mpsc;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A sink forwarding every update to the test's channel.
struct ChannelSink(mpsc::UnboundedSender<LoginUpdate>);

impl LoginSink for ChannelSink {
    fn update(&mut self, update: LoginUpdate) {
        let _ = self.0.send(update);
    }
}

/// A JWT whose payload is `claims`; the flow reads the payload only.
fn jwt(claims: serde_json::Value) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.signature")
}

/// The token endpoint's answer: a fresh pair and an ID token naming the
/// account.
fn token_body() -> String {
    serde_json::json!({
        "access_token": "new-access",
        "refresh_token": "new-refresh",
        "id_token": jwt(serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-9",
                "chatgpt_plan_type": "plus"
            }
        })),
        "expires_in": 3600
    })
    .to_string()
}

/// A flow against `server`, on an ephemeral callback port and without a
/// browser.
fn flow(server: &MockServer) -> ChatGptLogin {
    ChatGptLogin::new()
        .with_issuer(server.uri())
        .with_token_url(format!("{}/oauth/token", server.uri()))
        .with_client_id("test-client")
        .with_callback_port(0)
        .with_open_browser(false)
        .with_timeout(Duration::from_secs(5))
}

/// Spawns the flow and hands back the update receiver.
fn spawn(flow: ChatGptLogin) -> mpsc::UnboundedReceiver<LoginUpdate> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut sink = ChannelSink(tx);
        flow.run(&mut sink).await;
    });
    rx
}

/// The instruction URL's query parameters.
fn params(update: &LoginUpdate) -> Option<HashMap<String, String>> {
    let LoginUpdate::Instruct(instruction) = update else {
        return None;
    };
    let url = reqwest::Url::parse(instruction.url.as_deref()?).ok()?;
    Some(url.query_pairs().into_owned().collect())
}

#[tokio::test]
async fn the_flow_captures_the_browser_callback_and_reports_the_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=test-code"))
        .and(body_string_contains("client_id=test-client"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_string(token_body()))
        .expect(1)
        .mount(&server)
        .await;

    let mut updates = spawn(flow(&server));
    let first = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("the instruction arrives")
        .expect("the flow is alive");
    let LoginUpdate::Instruct(instruction) = &first else {
        panic!("the first update is the instruction: {first:?}");
    };
    assert!(instruction.text.contains("ChatGPT"), "{instruction:?}");
    let params = params(&first).expect("the URL has a query");
    let state = params.get("state").expect("state").clone();
    assert_eq!(
        params.get("client_id").map(String::as_str),
        Some("test-client")
    );
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let redirect = params.get("redirect_uri").expect("redirect");
    let port = reqwest::Url::parse(redirect)
        .expect("the redirect parses")
        .port()
        .expect("the callback port is there");

    // The browser's redirect: the flow answers after the exchange.
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/auth/callback?code=test-code&state={state}"
    ))
    .await
    .expect("the callback connects");
    assert_eq!(response.status(), 200);
    let page = response.text().await.unwrap();
    assert!(page.contains("Signed in"), "{page}");

    let credential = loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("the flow finishes")
            .expect("the channel stays open until the flow returns");
        match update {
            LoginUpdate::Completed(json) => break json,
            LoginUpdate::Failed(message) => panic!("the flow failed: {message}"),
            LoginUpdate::Instruct(_) | LoginUpdate::Waiting(_) => {}
        }
    };
    let credential = ChatGptCredential::parse(&credential);
    assert_eq!(credential.access_token, "new-access");
    assert_eq!(credential.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(credential.account_id.as_deref(), Some("acct-9"));
    assert_eq!(credential.email.as_deref(), Some("user@example.com"));
    assert_eq!(credential.plan.as_deref(), Some("plus"));
    assert!(credential.expires_at.is_some());
    server.verify().await;
}

#[tokio::test]
async fn a_mismatched_state_is_refused_without_exchanging_the_code() {
    let server = MockServer::start().await;
    // No token mock: an exchange attempt would be a 404 and a
    // different error; the state check must fail first.
    let mut updates = spawn(flow(&server));
    let first = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("the instruction arrives")
        .expect("the flow is alive");
    let params = params(&first).expect("the URL has a query");
    let redirect = params.get("redirect_uri").expect("redirect");
    let port = reqwest::Url::parse(redirect)
        .expect("the redirect parses")
        .port()
        .expect("the callback port is there");

    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/auth/callback?code=x&state=wrong"
    ))
    .await
    .expect("the callback connects");
    assert_eq!(response.status(), 400);

    let failed = loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("the flow finishes")
            .expect("the channel stays open until the flow returns");
        match update {
            LoginUpdate::Failed(message) => break message,
            LoginUpdate::Completed(_) => panic!("a wrong state must not complete"),
            LoginUpdate::Instruct(_) | LoginUpdate::Waiting(_) => {}
        }
    };
    assert!(
        failed.contains("state"),
        "the failure names the state check"
    );
}

#[tokio::test]
async fn a_provider_refusal_is_reported_inline() {
    let server = MockServer::start().await;
    let mut updates = spawn(flow(&server));
    let first = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("the instruction arrives")
        .expect("the flow is alive");
    let params = params(&first).expect("the URL has a query");
    let state = params.get("state").expect("state");
    let redirect = params.get("redirect_uri").expect("redirect");
    let port = reqwest::Url::parse(redirect)
        .expect("the redirect parses")
        .port()
        .expect("the callback port is there");

    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/auth/callback?error=access_denied&error_description=no+seat&state={state}"
    ))
    .await
    .expect("the callback connects");
    assert_eq!(response.status(), 400);

    let message = loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("the flow finishes")
            .expect("the channel stays open until the flow returns");
        match update {
            LoginUpdate::Failed(message) => break message,
            LoginUpdate::Completed(_) => panic!("a refusal must not complete"),
            LoginUpdate::Instruct(_) | LoginUpdate::Waiting(_) => {}
        }
    };
    assert!(message.contains("access_denied"), "{message}");
    assert!(message.contains("no seat"), "{message}");
}
