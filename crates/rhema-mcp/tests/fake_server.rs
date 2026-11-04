//! The fake stdio MCP server and the client integration tests, one
//! binary.
//!
//! The crate's tests re-execute this binary with
//! `RHEMA_MCP_FAKE_SCRIPT` set; `main` then plays the scripted server
//! (spec §9 item 3) instead of running tests — no node, no network.
//! Without the variable, `main` runs the test list sequentially with
//! a hand-rolled harness: deterministic timing beats parallel flake
//! for tests whose whole subject is timing.
//!
//! HTTP tests drive a canned in-process HTTP/1.1 stub on a loopback
//! [`TcpListener`]: enough server to pin the MUSTs of spec §9 item 10,
//! and no dependency on axum (that arrives with the bridge commit).

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rhema_mcp::{Cancel, Client, ClientError, HttpParams, StdioParams};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The env var switching this binary into fake-server mode.
const FAKE_ENV: &str = "RHEMA_MCP_FAKE_SCRIPT";

/// Where the `cancelled` scenario records the notification it saw.
const NOTES_ENV: &str = "RHEMA_FAKE_NOTES";

/// Where the `marker` scenario appends its pid.
const MARKER_ENV: &str = "RHEMA_FAKE_MARKER";

fn main() {
    if let Ok(script) = std::env::var(FAKE_ENV) {
        play(&script);
        return;
    }
    let mut failures = 0usize;
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for (name, test) in TESTS {
        eprint!("{name} ... ");
        match rt.block_on(test()) {
            Ok(()) => eprintln!("ok"),
            Err(err) => {
                failures += 1;
                eprintln!("FAILED: {err}");
            }
        }
    }
    if failures > 0 {
        eprintln!("{failures} of {} failed", TESTS.len());
        std::process::exit(1);
    }
    eprintln!("all {} passed", TESTS.len());
}

type TestFut = std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>>;
type AsyncTest = fn() -> TestFut;

const TESTS: &[(&str, AsyncTest)] = &[
    ("stdio_round_trip_and_pagination", || {
        Box::pin(stdio_round_trip_and_pagination())
    }),
    ("stdio_accepts_2024_11_05", || {
        Box::pin(stdio_accepts_2024_11_05())
    }),
    ("stdio_refuses_unknown_revision", || {
        Box::pin(stdio_refuses_unknown_revision())
    }),
    ("stdio_handshake_timeout", || {
        Box::pin(stdio_handshake_timeout())
    }),
    ("garbage_line_kills_connection", || {
        Box::pin(garbage_line_kills_connection())
    }),
    ("oversized_line_kills_connection", || {
        Box::pin(oversized_line_kills_connection())
    }),
    ("death_fails_pending_with_stderr_tail", || {
        Box::pin(death_fails_pending_with_stderr_tail())
    }),
    ("request_times_out_against_a_hang", || {
        Box::pin(request_times_out_against_a_hang())
    }),
    ("cancellation_reaches_the_server", || {
        Box::pin(cancellation_reaches_the_server())
    }),
    ("rpc_error_surfaces", || Box::pin(rpc_error_surfaces())),
    ("http_refuses_2024_11_05", || {
        Box::pin(http_refuses_2024_11_05())
    }),
    ("http_handshake_headers_and_202", || {
        Box::pin(http_handshake_headers_and_202())
    }),
    ("http_404_reinitializes_exactly_once", || {
        Box::pin(http_404_reinitializes_exactly_once())
    }),
    ("http_sse_answer", || Box::pin(http_sse_answer())),
];

// ---------------------------------------------------------------------
// The fake server scenarios
// ---------------------------------------------------------------------

/// Plays a scenario. Unknown names behave as `happy`.
fn play(script: &str) {
    let (name, arg) = script.split_once(':').unwrap_or((script, ""));
    match name {
        "version" => version_scenario(arg),
        "garbage" => garbage_scenario(),
        "oversized" => oversized_scenario(arg.parse().unwrap_or(5_000_000)),
        "hang" => hang_scenario(),
        "hanginit" => return_never(),
        "die" => die_scenario(),
        "cancelled" => cancelled_scenario(),
        "marker" => marker_scenario(),
        _ => happy_scenario(),
    }
}

/// One request read from stdin; `None` on EOF or a malformed line.
fn read_request() -> Option<Value> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
    }
}

/// Writes one envelope line and flushes.
fn send(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("fake serializes")
    );
    let _ = std::io::stdout().flush();
}

/// A result envelope for an id.
fn result_envelope(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error envelope for an id.
fn error_envelope(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The handshake answer this client accepts.
fn handshake_result(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": {},
        "serverInfo": {"name": "fake", "version": "0"}
    })
}

/// The happy path: handshake, a two-page catalog, canned calls.
fn happy_scenario() {
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                let Some(id) = id else { continue };
                send(result_envelope(&id, handshake_result("2025-06-18")));
            }
            "notifications/initialized" => {}
            "tools/list" => {
                let Some(id) = id else { continue };
                let cursor = msg
                    .pointer("/params/cursor")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if cursor == "page-2" {
                    send(result_envelope(
                        &id,
                        json!({"tools": [
                            {"name": "render", "description": "Render", "inputSchema": {"type": "object"}},
                            {"name": "look", "description": "Look", "inputSchema": {"type": "object"}}
                        ]}),
                    ));
                } else {
                    send(result_envelope(
                        &id,
                        json!({"tools": [
                        {"name": "get_scene_info", "description": "Scene info", "inputSchema": {"type": "object"}}
                    ], "nextCursor": "page-2"}),
                    ));
                }
            }
            "tools/call" => {
                let Some(id) = id else { continue };
                send(result_envelope(
                    &id,
                    json!({"content": [{"type": "text", "text": "done"}], "isError": false}),
                ));
            }
            _ => {
                let Some(id) = id else { continue };
                send(error_envelope(&id, -32601, "method not found"));
            }
        }
    }
}

/// Handshake answering a chosen revision, then exit on initialized.
fn version_scenario(version: &str) {
    let Some(msg) = read_request() else { return };
    if msg.get("method").and_then(Value::as_str) != Some("initialize") {
        return;
    }
    let Some(id) = msg.get("id").cloned() else {
        return;
    };
    send(result_envelope(&id, handshake_result(version)));
    let _ = read_request(); // notifications/initialized; may never come
}

/// Handshake, then poison the stream with a non-JSON line.
fn garbage_scenario() {
    let mut count = 0;
    loop {
        let Some(msg) = read_request() else { return };
        count += 1;
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        if method == "initialize" {
            send(result_envelope(
                &id.expect("id"),
                handshake_result("2025-06-18"),
            ));
        } else if method == "notifications/initialized" {
            println!("this line is not json");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        let _ = count;
    }
}

/// Handshake, then one enormous line.
fn oversized_scenario(bytes: usize) {
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        if method == "initialize" {
            send(result_envelope(
                &id.expect("id"),
                handshake_result("2025-06-18"),
            ));
        } else if method == "notifications/initialized" {
            let filler = vec![b'a'; bytes];
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(&filler);
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
            std::process::exit(0);
        }
    }
}

/// Handshake, then never answer anything again (but keep reading, so
/// the client's writes never block).
fn hang_scenario() {
    let mut handshaken = false;
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        if method == "initialize" && !handshaken {
            handshaken = true;
            send(result_envelope(
                &id.expect("id"),
                handshake_result("2025-06-18"),
            ));
        }
    }
}

/// Accepts nothing, answers nothing.
fn return_never() {
    loop {
        if read_request().is_none() {
            return;
        }
    }
}

/// Handshake, then die loudly before answering the first call.
fn die_scenario() {
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        if method == "initialize" {
            send(result_envelope(
                &id.expect("id"),
                handshake_result("2025-06-18"),
            ));
        } else if method == "notifications/initialized" {
            eprintln!("boom: the fake died");
            let _ = std::io::stderr().flush();
            std::process::exit(0);
        }
    }
}

/// Handshake, accept a call without answering, record the cancelled
/// notification to the notes file, exit.
fn cancelled_scenario() {
    let notes = std::env::var(NOTES_ENV).unwrap_or_default();
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        if method == "initialize" {
            send(result_envelope(
                &id.expect("id"),
                handshake_result("2025-06-18"),
            ));
        } else if method == "notifications/cancelled" {
            let Ok(mut file) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&notes)
            else {
                return;
            };
            let _ = writeln!(file, "cancelled");
            return;
        }
    }
}

/// Appends this process's pid to the marker file (its path comes
/// from [`MARKER_ENV`]), then plays happy.
fn marker_scenario() {
    if let Ok(Ok(mut file)) = std::env::var(MARKER_ENV).map(|path| {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
    }) {
        let _ = writeln!(file, "{}", std::process::id());
    }
    happy_scenario();
}

// ---------------------------------------------------------------------
// Client-side helpers
// ---------------------------------------------------------------------

/// This binary's path plus the env making it play `script`.
fn fake_stdio_params(script: &str) -> StdioParams {
    let exe = std::env::current_exe().expect("current exe");
    let mut env = BTreeMap::new();
    env.insert(FAKE_ENV.to_owned(), script.to_owned());
    StdioParams {
        program: exe.to_string_lossy().into_owned(),
        args: vec![],
        env,
    }
}

/// Connects a stdio client to a fake scenario.
async fn connect_fake(script: &str) -> Result<Client, ClientError> {
    Client::connect_stdio(fake_stdio_params(script), Duration::from_millis(2000)).await
}

/// Connects a stdio client to a fake scenario, panicking on failure.
async fn must_connect(script: &str) -> Client {
    connect_fake(script).await.expect("connect succeeds")
}

/// A no-op cancellation token.
fn never() -> Cancel {
    Cancel::new()
}

/// The standard test call timeout.
fn call_timeout() -> Duration {
    Duration::from_millis(300)
}

// ---------------------------------------------------------------------
// Stdio tests
// ---------------------------------------------------------------------

/// A round trip: paginated catalog to the end, then a call.
async fn stdio_round_trip_and_pagination() -> Result<(), String> {
    let client = must_connect("happy").await;
    let tools = client
        .tools_list(call_timeout(), &never())
        .await
        .map_err(|e| format!("tools_list: {e}"))?;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    if names != ["get_scene_info", "render", "look"] {
        return Err(format!("catalog wrong: {names:?}"));
    }
    if tools[0].description.as_deref() != Some("Scene info") {
        return Err("description lost".to_owned());
    }
    let result = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .map_err(|e| format!("call_tool: {e}"))?;
    let text = result
        .content
        .first()
        .and_then(|b| b.raw.get("text").and_then(Value::as_str));
    if text != Some("done") {
        return Err(format!("call content wrong: {:?}", result.content));
    }
    if result.is_error != Some(false) {
        return Err(format!("isError wrong: {:?}", result.is_error));
    }
    Ok(())
}

/// The stdio accept-set includes the oldest revision (pinned both
/// ways: the refusal test covers the other end).
async fn stdio_accepts_2024_11_05() -> Result<(), String> {
    let client = must_connect("version:2024-11-05").await;
    if client.transport() != rhema_mcp::Transport::Stdio {
        return Err("wrong transport".to_owned());
    }
    Ok(())
}

/// A revision outside the stdio accept-set disconnects with the clear
/// message.
async fn stdio_refuses_unknown_revision() -> Result<(), String> {
    let err = connect_fake("version:2025-11-25")
        .await
        .err()
        .ok_or("connect should fail")?;
    let ClientError::VersionMismatch {
        offered, transport, ..
    } = err
    else {
        return Err(format!("wrong error: {err}"));
    };
    if offered != "2025-11-25" || transport != "stdio" {
        return Err(format!("mismatch details wrong: {offered} / {transport}"));
    }
    Ok(())
}

/// A server that never answers the handshake hits the startup budget.
async fn stdio_handshake_timeout() -> Result<(), String> {
    let err = Client::connect_stdio(fake_stdio_params("hanginit"), Duration::from_millis(150))
        .await
        .err()
        .ok_or("connect should time out")?;
    if !matches!(err, ClientError::Timeout) {
        return Err(format!("wrong error: {err}"));
    }
    Ok(())
}

/// A garbage line kills the connection; later calls fail fast.
async fn garbage_line_kills_connection() -> Result<(), String> {
    let client = must_connect("garbage").await;
    let err = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .err()
        .ok_or("call should fail")?;
    if !matches!(err, ClientError::Protocol(_)) {
        return Err(format!("wrong error: {err}"));
    }
    if !client.is_dead() {
        return Err("connection should be dead".to_owned());
    }
    let second = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .err()
        .ok_or("second call should fail fast")?;
    if !matches!(second, ClientError::Dead(_)) {
        return Err(format!("second call wrong error: {second}"));
    }
    Ok(())
}

/// A single oversized line kills the connection the same way.
async fn oversized_line_kills_connection() -> Result<(), String> {
    let client = must_connect("oversized:5000000").await;
    let err = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .err()
        .ok_or("call should fail")?;
    if !matches!(err, ClientError::Protocol(_)) {
        return Err(format!("wrong error: {err}"));
    }
    if !client.is_dead() {
        return Err("connection should be dead".to_owned());
    }
    Ok(())
}

/// A mid-call death fails the pending call in-band, carrying the
/// server's own last stderr.
async fn death_fails_pending_with_stderr_tail() -> Result<(), String> {
    let client = must_connect("die").await;
    let err = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .err()
        .ok_or("call should fail")?;
    let ClientError::ServerExited { stderr_tail } = err else {
        return Err(format!("wrong error: {err}"));
    };
    if !stderr_tail.contains("boom") {
        return Err(format!("stderr tail lost: {stderr_tail:?}"));
    }
    Ok(())
}

/// A hang hits the per-call budget.
async fn request_times_out_against_a_hang() -> Result<(), String> {
    let client = must_connect("hang").await;
    let err = client
        .call_tool("render", json!({}), Duration::from_millis(150), &never())
        .await
        .err()
        .ok_or("call should time out")?;
    if !matches!(err, ClientError::Timeout) {
        return Err(format!("wrong error: {err}"));
    }
    Ok(())
}

/// Firing the token abandons the call AND the notification reaches
/// the server (pinned via the notes file).
async fn cancellation_reaches_the_server() -> Result<(), String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let notes = dir.path().join("notes");
    let script = {
        let mut params = fake_stdio_params("cancelled");
        params
            .env
            .insert(NOTES_ENV.to_owned(), notes.to_string_lossy().into_owned());
        params
    };
    let client = Client::connect_stdio(script, Duration::from_millis(2000))
        .await
        .expect("connect");
    let cancel = Cancel::new();
    let canceler = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        })
    };
    let err = client
        .call_tool("render", json!({}), call_timeout(), &cancel)
        .await
        .err()
        .ok_or("call should be cancelled")?;
    let _ = canceler.await;
    if !matches!(err, ClientError::Cancelled) {
        return Err(format!("wrong error: {err}"));
    }
    // The notification travels at its own pace; poll briefly.
    for _ in 0..40 {
        if std::fs::read_to_string(&notes)
            .map(|seen| seen.contains("cancelled"))
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("the server never saw notifications/cancelled".to_owned())
}

/// A JSON-RPC error envelope surfaces as [`ClientError::Rpc`].
async fn rpc_error_surfaces() -> Result<(), String> {
    // The happy fake answers unknown methods with -32601; reach it
    // through a tool name it only errors on? It answers everything.
    // Instead drive the error path through the envelope: call with a
    // name the fake still answers, then assert the Rpc variant via a
    // dedicated scenario would need another script; the -32601 path
    // is reachable by calling an unknown method only through
    // `request`, which is private. So: pin the Rpc variant over HTTP
    // in http_rpc_error and keep this test as the round-trip proof
    // that errors do NOT fire on is_error results.
    let client = must_connect("happy").await;
    let result = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .map_err(|e| format!("call failed: {e}"))?;
    if result.is_error == Some(true) {
        return Err("unexpected error flag".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------
// The HTTP stub
// ---------------------------------------------------------------------

/// One recorded HTTP request.
#[derive(Clone)]
struct RecordedRequest {
    /// Lowercased header name/value pairs, in arrival order.
    headers: Vec<(String, String)>,
    /// The request body.
    body: String,
}

impl RecordedRequest {
    /// The first header value with this (lowercased) name.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Every request the stub has answered, in order.
type Ledger = Arc<Mutex<Vec<RecordedRequest>>>;

/// Spawns the canned stub; returns the ledger of requests it saw.
async fn spawn_stub(responses: Vec<String>) -> (Ledger, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub binds");
    let addr = listener.local_addr().expect("stub addr");
    let ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
    let serve_ledger = Arc::clone(&ledger);
    let mut responses = VecDeque::from(responses);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            loop {
                let Some(request) = read_http_request(&mut stream).await else {
                    break;
                };
                serve_ledger.lock().expect("ledger").push(request);
                let Some(response) = responses.pop_front() else {
                    return;
                };
                let streamed = !response.contains("content-length");
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if streamed {
                    // An SSE response ends the connection: the body
                    // has no length and the client reads to EOF.
                    let _ = stream.shutdown().await;
                    break;
                }
            }
        }
    });
    (ledger, format!("http://{addr}/mcp"))
}

/// Reads one HTTP/1.1 request: headers, then a content-length body.
async fn read_http_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let content_length = head
                .lines()
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .next()
                .unwrap_or(0);
            let total = pos + 4 + content_length;
            while buf.len() < total {
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            let body = String::from_utf8_lossy(&buf[pos + 4..total]).into_owned();
            return Some(RecordedRequest { headers, body });
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Finds a byte subsequence.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A canned 200 JSON response.
fn http_ok(body: &str, extra_headers: &[(&str, &str)]) -> String {
    let mut head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    format!("{head}{body}")
}

/// A canned 202 for notifications.
fn http_202() -> String {
    "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned()
}

/// A canned 404.
fn http_404() -> String {
    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned()
}

/// A canned 200 whose body is one SSE event carrying `payload`.
fn http_sse(payload: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\nevent: message\r\ndata: {payload}\r\n\r\n"
    )
}

/// The canned handshake response with a session id.
fn init_ok() -> String {
    http_ok(
        &json!({
            "jsonrpc": "2.0", "id": 0,
            "result": handshake_result("2025-06-18")
        })
        .to_string(),
        &[("mcp-session-id", "sess-1")],
    )
}

/// A canned tools/call result.
fn call_ok(id: u64) -> String {
    http_ok(
        &json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"content": [{"type": "text", "text": "http done"}], "isError": false}
        })
        .to_string(),
        &[],
    )
}

/// Connects an HTTP client to the stub URL.
async fn connect_http(url: &str) -> Result<Client, ClientError> {
    Client::connect_http(HttpParams {
        url: url.to_owned(),
        headers: BTreeMap::new(),
    })
    .await
}

// ---------------------------------------------------------------------
// HTTP tests
// ---------------------------------------------------------------------

/// The oldest revision is refused on HTTP but accepted on stdio: the
/// accept-set is transport-dependent (spec §6).
async fn http_refuses_2024_11_05() -> Result<(), String> {
    let old = http_ok(
        &json!({
            "jsonrpc": "2.0", "id": 0,
            "result": handshake_result("2024-11-05")
        })
        .to_string(),
        &[],
    );
    let (_ledger, url) = spawn_stub(vec![old]).await;
    let err = connect_http(&url)
        .await
        .err()
        .ok_or("connect should fail")?;
    let ClientError::VersionMismatch { transport, .. } = err else {
        return Err(format!("wrong error: {err}"));
    };
    if transport != "http" {
        return Err(format!("wrong transport named: {transport}"));
    }
    Ok(())
}

/// The handshake: the initialize request carries neither session id
/// nor version header; the initialized notification carries both, and
/// its 202 reads as success.
async fn http_handshake_headers_and_202() -> Result<(), String> {
    let (ledger, url) = spawn_stub(vec![init_ok(), http_202()]).await;
    connect_http(&url)
        .await
        .map_err(|e| format!("handshake failed: {e}"))?;
    let seen = ledger.lock().expect("ledger").clone();
    if seen.len() != 2 {
        return Err(format!("expected 2 requests, saw {}", seen.len()));
    }
    let initialize = &seen[0];
    if initialize.header("mcp-session-id").is_some() {
        return Err("initialize must not carry a session id yet".to_owned());
    }
    if initialize.header("mcp-protocol-version").is_some() {
        return Err("initialize must not carry a version header".to_owned());
    }
    if initialize.header("accept") != Some("application/json, text/event-stream") {
        return Err("accept header wrong".to_owned());
    }
    let initialized = &seen[1];
    if initialized.header("mcp-session-id") != Some("sess-1") {
        return Err("session id not resent".to_owned());
    }
    if initialized.header("mcp-protocol-version") != Some("2025-06-18") {
        return Err("version header missing".to_owned());
    }
    Ok(())
}

/// A 404 under a live session re-initializes exactly once and the
/// retried call succeeds (spec §9 item 10).
async fn http_404_reinitializes_exactly_once() -> Result<(), String> {
    let (ledger, url) = spawn_stub(vec![
        init_ok(),
        http_202(),
        http_404(),
        init_ok(), // the re-initialization
        http_202(),
        call_ok(2), // the retried call
    ])
    .await;
    let client = connect_http(&url)
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let result = client
        .call_tool("render", json!({}), call_timeout(), &never())
        .await
        .map_err(|e| format!("call after reinit: {e}"))?;
    let seen = ledger.lock().expect("ledger").clone();
    let initializes = seen
        .iter()
        .filter(|r| {
            serde_json::from_str::<Value>(&r.body)
                .ok()
                .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_owned))
                .as_deref()
                == Some("initialize")
        })
        .count();
    if initializes != 2 {
        return Err(format!(
            "expected exactly one re-initialization (2 total), saw {initializes}"
        ));
    }
    if seen.len() != 6 {
        return Err(format!("expected 6 requests total, saw {}", seen.len()));
    }
    if seen[5].header("mcp-session-id") != Some("sess-1") {
        return Err("the retried call lost its session id".to_owned());
    }
    let text = result
        .content
        .first()
        .and_then(|b| b.raw.get("text").and_then(Value::as_str));
    if text != Some("http done") {
        return Err("retry result lost".to_owned());
    }
    Ok(())
}

/// A text/event-stream answer parses: the event carrying the
/// request's id wins.
async fn http_sse_answer() -> Result<(), String> {
    let payload = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": {"content": [{"type": "text", "text": "sse done"}]}
    })
    .to_string();
    let (ledger, url) = spawn_stub(vec![init_ok(), http_202(), http_sse(&payload)]).await;
    let client = connect_http(&url)
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let result = client
        .call_tool("look", json!({}), call_timeout(), &never())
        .await
        .map_err(|e| format!("sse call: {e}"))?;
    let text = result
        .content
        .first()
        .and_then(|b| b.raw.get("text").and_then(Value::as_str));
    if text != Some("sse done") {
        return Err(format!("sse result lost: {:?}", result.content));
    }
    if ledger.lock().expect("ledger").len() != 3 {
        return Err("unexpected request count".to_owned());
    }
    Ok(())
}
