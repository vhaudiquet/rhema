//! The conformance suite: every guarantee the contract names, phrased
//! as requests against an opaque transport. The daemon's real router
//! and the mock router both answer it; same results is the definition
//! of the daemon implementing the contract.

use crate::{ApiError, Auth, CallRequest, CallResponse, now_millis};

/// Runs every check against `call`, which performs one HTTP request.
/// The first failure is returned, named by the check that failed; `Ok`
/// means the transport speaks the contract.
///
/// The suite assumes a daemon configured with both tokens — a human
/// one and an agent one — because the agent-token class is the
/// contract's load-bearing guarantee. A daemon running an open surface
/// (no tokens) cannot pass it, by design: the credential section of
/// the spec (§5) makes the scoped agent token a precondition of the
/// client tools.
pub async fn conformance<F, Fut>(call: F) -> Result<(), String>
where
    F: Fn(CallRequest) -> Fut,
    Fut: std::future::Future<Output = Result<CallResponse, String>>,
{
    let ctx = Ctx { call: &call };

    // — The bearer gate —
    ctx.expect(
        "a missing bearer is rejected",
        req("GET", "/v1/watches", Auth::None, Some(1), None),
        401,
    )
    .await?;
    ctx.expect(
        "a wrong bearer is rejected",
        req("GET", "/v1/watches", Auth::Invalid, Some(1), None),
        401,
    )
    .await?;

    // — The contract version handshake —
    ctx.expect(
        "an agent call without the contract header conflicts",
        req("GET", "/v1/watches", Auth::Agent, None, None),
        409,
    )
    .await?;
    ctx.expect(
        "an agent call at a foreign contract version conflicts",
        req("GET", "/v1/watches", Auth::Agent, Some(999), None),
        409,
    )
    .await?;

    // — Watches: add, see it listed, remove —
    ctx.expect(
        "the agent lists watches",
        req("GET", "/v1/watches", Auth::Agent, Some(1), None),
        200,
    )
    .await?;
    let added = ctx
        .expect_success(
            "the agent adds a watch",
            req(
                "POST",
                "/v1/watches",
                Auth::Agent,
                Some(1),
                Some(serde_json::json!({
                    "forge": "github", "repo": "acme/widgets"
                })),
            ),
        )
        .await?;
    if added.body.to_string().contains("error") {
        return Err("the agent adds a watch: the answer reads as an error".into());
    }
    let listed = ctx
        .expect(
            "the agent lists watches again",
            req("GET", "/v1/watches", Auth::Agent, Some(1), None),
            200,
        )
        .await?;
    if !listed.body.to_string().contains("acme/widgets") {
        return Err("the watch the agent added is not in the list".into());
    }
    ctx.expect_success(
        "the agent removes the watch",
        req(
            "DELETE",
            "/v1/watches/github/acme/widgets",
            Auth::Agent,
            Some(1),
            None,
        ),
    )
    .await?;

    // — Notifications: push with and without a device —
    let pushed = ctx
        .expect_success(
            "the agent pushes a notification",
            req(
                "POST",
                "/v1/notifications",
                Auth::Agent,
                Some(1),
                Some(serde_json::json!({
                    "title": "check", "body": "the build", "device": "phone"
                })),
            ),
        )
        .await?;
    if pushed
        .body
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .is_none()
    {
        return Err("a pushed notification answers with its row id".into());
    }
    if pushed
        .body
        .get("device")
        .and_then(serde_json::Value::as_str)
        != Some("phone")
    {
        return Err("a pushed notification echoes its device".into());
    }
    let defaulted = ctx
        .expect_success(
            "a device-less push lands on the default device",
            req(
                "POST",
                "/v1/notifications",
                Auth::Agent,
                Some(1),
                Some(serde_json::json!({
                    "title": "check", "body": "the build"
                })),
            ),
        )
        .await?;
    if defaulted
        .body
        .get("device")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err("a device-less push answers a non-empty device".into());
    }

    // — Tasks: schedule both kinds, list, remove, remove again —
    let at = ctx
        .expect_success(
            "the agent schedules a one-shot task",
            req(
                "POST",
                "/v1/tasks",
                Auth::Agent,
                Some(1),
                Some(serde_json::json!({
                    "kind": "at", "prompt": "check CI", "at_millis": now_millis() + 60_000
                })),
            ),
        )
        .await?;
    let at_id = at
        .body
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or("a scheduled task answers with its row id")?;
    ctx.expect_success(
        "the agent schedules a recurring task",
        req(
            "POST",
            "/v1/tasks",
            Auth::Agent,
            Some(1),
            Some(serde_json::json!({
                "kind": "every", "prompt": "water the plants", "interval_millis": 3_600_000
            })),
        ),
    )
    .await?;
    let listed = ctx
        .expect(
            "the agent lists tasks",
            req("GET", "/v1/tasks", Auth::Agent, Some(1), None),
            200,
        )
        .await?;
    let ids: Vec<i64> = listed
        .body
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|t| t.get("id").and_then(serde_json::Value::as_i64))
                .collect()
        })
        .unwrap_or_default();
    if !ids.contains(&at_id) {
        return Err("the task the agent scheduled is not in the list".into());
    }
    let removed = ctx
        .expect_success(
            "the agent cancels the task",
            req(
                "DELETE",
                &format!("/v1/tasks/{at_id}"),
                Auth::Agent,
                Some(1),
                None,
            ),
        )
        .await?;
    if removed
        .body
        .get("removed")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err("cancelling an existing task answers removed: true".into());
    }
    let again = ctx
        .expect_success(
            "cancelling a cancelled task still answers ok",
            req(
                "DELETE",
                &format!("/v1/tasks/{at_id}"),
                Auth::Agent,
                Some(1),
                None,
            ),
        )
        .await?;
    if again
        .body
        .get("removed")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return Err("cancelling a gone task answers removed: false".into());
    }

    // — The class a malformed body meets: a client error. The exact
    // code is the server's framework's business (400 or 422); the
    // contract only fixes the class.
    let malformed = ctx
        .send(
            "a malformed task body",
            req(
                "POST",
                "/v1/tasks",
                Auth::Agent,
                Some(1),
                Some(serde_json::json!({})),
            ),
        )
        .await?;
    if !(400..500).contains(&malformed.status) {
        return Err(format!(
            "a malformed task body is a client error, got {} ({})",
            malformed.status, malformed.body
        ));
    }

    // — The allowlist's denials: the routes a tool must never reach —
    ctx.expect(
        "the agent cannot answer a consent",
        req(
            "POST",
            "/v1/consents/c-1",
            Auth::Agent,
            Some(1),
            Some(serde_json::json!({"grant": true})),
        ),
        403,
    )
    .await?;
    ctx.expect(
        "the agent cannot ingest an event",
        req(
            "POST",
            "/v1/ingest",
            Auth::Agent,
            Some(1),
            Some(serde_json::json!({
                "source": "x", "key": "y", "payload": {}
            })),
        ),
        403,
    )
    .await?;
    ctx.expect(
        "the agent cannot create a session",
        req(
            "POST",
            "/v1/sessions",
            Auth::Agent,
            Some(1),
            Some(serde_json::json!({})),
        ),
        403,
    )
    .await?;
    ctx.expect(
        "the agent cannot forget a phone",
        req("DELETE", "/v1/phones/p1", Auth::Agent, Some(1), None),
        403,
    )
    .await?;

    // — The human token is untouched: the full surface —
    ctx.expect(
        "the human lists consents",
        req("GET", "/v1/consents", Auth::Human, None, None),
        200,
    )
    .await?;
    ctx.expect_success(
        "the human ingests an event",
        req(
            "POST",
            "/v1/ingest",
            Auth::Human,
            None,
            Some(serde_json::json!({
                "source": "ciwatch", "key": "ci:build/1", "payload": {"text": "ping"}
            })),
        ),
    )
    .await?;

    // — The one error shape —
    let missing = ctx
        .expect(
            "an unknown route answers the error shape",
            req("GET", "/v1/no-such-route", Auth::Human, None, None),
            404,
        )
        .await?;
    if serde_json::from_value::<ApiError>(missing.body.clone()).is_err() {
        return Err("every error answer is {\"error\": \"...\"}".into());
    }

    Ok(())
}

/// The suite's transport: the closure, plus named checks over it.
struct Ctx<'a, F, Fut>
where
    F: Fn(CallRequest) -> Fut,
    Fut: std::future::Future<Output = Result<CallResponse, String>>,
{
    call: &'a F,
}

impl<'a, F, Fut> Ctx<'a, F, Fut>
where
    F: Fn(CallRequest) -> Fut,
    Fut: std::future::Future<Output = Result<CallResponse, String>>,
{
    /// Performs one request and requires its status; anything else is
    /// the check's failure, spelled with the check's name.
    async fn expect(
        &self,
        name: &str,
        request: CallRequest,
        status: u16,
    ) -> Result<CallResponse, String> {
        let got = self.send(name, request).await?;
        if got.status != status {
            return Err(format!(
                "{name}: expected {status}, got {} ({})",
                got.status, got.body
            ));
        }
        Ok(got)
    }

    /// Performs one request and requires success — any 2xx. Creations
    /// may answer 200 or 201; the contract does not pick for the
    /// server. Denials stay exact in [`expect`].
    async fn expect_success(
        &self,
        name: &str,
        request: CallRequest,
    ) -> Result<CallResponse, String> {
        let got = self.send(name, request).await?;
        if !(200..300).contains(&got.status) {
            return Err(format!(
                "{name}: expected a success, got {} ({})",
                got.status, got.body
            ));
        }
        Ok(got)
    }

    /// Performs one request, answering whatever came back; the caller
    /// judges. A transport failure is the check's failure.
    async fn send(&self, name: &str, request: CallRequest) -> Result<CallResponse, String> {
        (self.call)(request)
            .await
            .map_err(|err| format!("{name}: the transport failed: {err}"))
    }
}

/// Builds one request with the shared defaults.
fn req(
    method: &'static str,
    path: &str,
    auth: Auth,
    contract: Option<u32>,
    body: Option<serde_json::Value>,
) -> CallRequest {
    CallRequest {
        method,
        path: path.to_owned(),
        auth,
        contract,
        body,
    }
}
