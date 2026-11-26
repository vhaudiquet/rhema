//! The session's host endpoint: one Unix socket where the sandbox's
//! `search` and `decide` shell functions hand their argv to the host.
//!
//! The endpoint is the host half of those two macros. The shell
//! function carries no key and no provider config; it sends the
//! request to this process, and the process resolves the profile's
//! providers and keys in memory. A backend that shares a path with the
//! host (local, Docker, LXD, ecrou) reaches the socket directly. SSH
//! asks the backend for a reverse socket forward. A backend with no
//! path to the host gets no endpoint, and its macros stay off.
//!
//! The wire is a tiny HTTP/1.1 exchange over one request per
//! connection: `POST /v1/search` or `POST /v1/decide` with a JSON body
//! `{"argv": [...], "stdin": "..."}`, answered with
//! `{"code": n, "stdout": "...", "stderr": "..."}`. The bearer token
//! scopes the socket to the session that provisioned it.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use rand::Rng as _;
use rhema_bash::runtime::{VAR_CAP_SOCKET, VAR_CAP_TOKEN};
use rhema_config::DecisionInstance;

use crate::sandbox::Sandboxed;

/// The largest request the endpoint reads: a state plus questions is
/// text, and an unbounded read of a misdirected pipe is a hang.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// The accept poll interval. A non-blocking listener cannot be woken
/// by a flag, so the thread looks at the flag at this rate.
const ACCEPT_TICK: Duration = Duration::from_millis(50);

/// The endpoint's two handlers. A session may wire one and not the
/// other; the missing path answers an in-band "not available".
#[derive(Default)]
struct Handlers {
    search: Option<Arc<rhema_websearch::Chain>>,
    decision: Option<DecisionSetup>,
}

/// One decision handler: the instance and key the host resolved.
#[derive(Clone)]
pub(crate) struct DecisionSetup {
    pub instance: DecisionInstance,
    pub key: Option<String>,
}

/// What one feature contributes to the endpoint.
pub(crate) enum Block {
    Search(Arc<rhema_websearch::Chain>),
    Decision(DecisionSetup),
}

/// One session's endpoint, held alive by the wirings that use it.
pub(crate) struct Endpoint {
    /// The socket as the sandbox sees it.
    pub socket: String,
    /// The bearer token the sandbox must present.
    pub token: String,
    inner: Arc<Inner>,
    host_socket: PathBuf,
    /// The temp directory, when the endpoint is host-only.
    _local: Option<tempfile::TempDir>,
}

struct Inner {
    stop: AtomicBool,
    handlers: Mutex<Handlers>,
}

impl Endpoint {
    /// The env entries a shell needs to reach this endpoint.
    pub(crate) fn env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (VAR_CAP_SOCKET.to_owned(), self.socket.clone()),
            (VAR_CAP_TOKEN.to_owned(), self.token.clone()),
        ])
    }

    fn add(&self, block: Block) {
        let mut handlers = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match block {
            Block::Search(chain) => handlers.search = Some(chain),
            Block::Decision(setup) => handlers.decision = Some(setup),
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop once so it observes the flag promptly.
        let _ = UnixStream::connect(&self.host_socket);
        let _ = std::fs::remove_file(&self.host_socket);
    }
}

static ENDPOINTS: OnceLock<Mutex<BTreeMap<String, Weak<Endpoint>>>> = OnceLock::new();
static NOTICED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<String, Weak<Endpoint>>> {
    ENDPOINTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn notice(session: &str, message: &str) {
    let mut seen = NOTICED
        .get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.insert(session.to_owned()) {
        eprintln!("warning: {message}");
    }
}

/// The session's endpoint, started when it has no live one and given
/// this feature's handler. `None` when the backend has no path to the
/// host.
///
/// A shared mount serves the socket at the same path on both sides.
/// A local session reaches a host temp socket. SSH asks the backend to
/// forward a remote socket to the host. HTTP has no route yet, and
/// neither does a sandbox whose forward fails.
pub(crate) fn wire(
    session_key: &str,
    sandboxed: Option<&Sandboxed>,
    block: Block,
) -> Option<Arc<Endpoint>> {
    if let Some(endpoint) = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_key)
        .and_then(Weak::upgrade)
    {
        endpoint.add(block);
        return Some(endpoint);
    }

    let runtime_key = crate::shared_kit::runtime_session_key(session_key);
    let (host_socket, sandbox_socket, local) = match sandboxed {
        Some(sandboxed) => match sandboxed.shared() {
            Some(shared) => {
                let dir = shared.session_dir(&runtime_key);
                if let Err(err) = std::fs::create_dir_all(&dir) {
                    notice(
                        session_key,
                        &format!("search and decide are off: the cap directory: {err}"),
                    );
                    return None;
                }
                let host = dir.join("cap.sock");
                let sandbox = Path::new(&shared.sandbox)
                    .join("sessions")
                    .join(&runtime_key)
                    .join("cap.sock");
                (host, sandbox.to_string_lossy().into_owned(), None)
            }
            None => {
                let dir = match tempfile::tempdir() {
                    Ok(dir) => dir,
                    Err(err) => {
                        notice(
                            session_key,
                            &format!("search and decide are off: the cap temp dir: {err}"),
                        );
                        return None;
                    }
                };
                let socket = dir.path().join("cap.sock");
                (
                    socket.clone(),
                    socket.to_string_lossy().into_owned(),
                    Some(dir),
                )
            }
        },
        None => {
            let dir = match tempfile::tempdir() {
                Ok(dir) => dir,
                Err(err) => {
                    notice(
                        session_key,
                        &format!("search and decide are off: the cap temp dir: {err}"),
                    );
                    return None;
                }
            };
            let socket = dir.path().join("cap.sock");
            (
                socket.clone(),
                socket.to_string_lossy().into_owned(),
                Some(dir),
            )
        }
    };

    if let Some(sandboxed) = sandboxed
        && sandboxed.shared().is_none()
    {
        if sandboxed.choice == crate::args::SandboxChoice::Http {
            notice(
                session_key,
                "search and decide are off: the http sandbox has no route to the host endpoint",
            );
            return None;
        }
        let Some(sandbox) = sandboxed.sandbox.clone() else {
            notice(
                session_key,
                "search and decide are off: this sandbox has no forward path to the host",
            );
            return None;
        };
        let remote = remote_socket(&runtime_key);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            notice(
                session_key,
                "search and decide are off: no async runtime to establish the sandbox forward",
            );
            return None;
        };
        // The forward is a connection of its own, held by the SSH
        // backend's sandbox; the shell retries until its socket
        // appears, so setup does not wait for the round trip.
        let task_sandbox = sandbox.clone();
        let task_remote = remote.clone();
        let task_host = host_socket.clone();
        let task_session = session_key.to_owned();
        runtime.spawn(async move {
            if let Err(err) = task_sandbox
                .forward_host_call(&task_remote, &task_host)
                .await
            {
                notice(
                    &task_session,
                    &format!(
                        "search and decide are off: the sandbox could not forward the host endpoint: {err}"
                    ),
                );
            }
        });
        let host_socket = host_socket.clone();
        let token = token();
        let inner = Arc::new(Inner {
            stop: AtomicBool::new(false),
            handlers: Mutex::new(Handlers::default()),
        });
        let endpoint = match Endpoint::start(host_socket, remote, token, inner, local) {
            Ok(endpoint) => Arc::new(endpoint),
            Err(err) => {
                notice(
                    session_key,
                    &format!("search and decide are off: the host endpoint: {err}"),
                );
                return None;
            }
        };
        endpoint.add(block);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_key.to_owned(), Arc::downgrade(&endpoint));
        return Some(endpoint);
    }

    let token = token();
    let inner = Arc::new(Inner {
        stop: AtomicBool::new(false),
        handlers: Mutex::new(Handlers::default()),
    });
    let endpoint = match Endpoint::start(host_socket, sandbox_socket, token, inner, local) {
        Ok(endpoint) => Arc::new(endpoint),
        Err(err) => {
            notice(
                session_key,
                &format!("search and decide are off: the host endpoint: {err}"),
            );
            return None;
        }
    };
    endpoint.add(block);
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_key.to_owned(), Arc::downgrade(&endpoint));
    Some(endpoint)
}

impl Endpoint {
    fn start(
        host_socket: PathBuf,
        sandbox_socket: String,
        token: String,
        inner: Arc<Inner>,
        local: Option<tempfile::TempDir>,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(&host_socket);
        let listener = UnixListener::bind(&host_socket)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&host_socket, std::fs::Permissions::from_mode(0o600))?;
        }
        listener.set_nonblocking(true)?;
        let thread_inner = inner.clone();
        let thread_token = token.clone();
        std::thread::Builder::new()
            .name("rhema-cap".into())
            .spawn(move || accept_loop(listener, thread_inner, thread_token))?;
        Ok(Self {
            socket: sandbox_socket,
            token,
            inner,
            host_socket,
            _local: local,
        })
    }
}

fn accept_loop(listener: UnixListener, inner: Arc<Inner>, token: String) {
    while !inner.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let inner = inner.clone();
                let token = token.clone();
                let _ = std::thread::Builder::new()
                    .name("rhema-cap-call".into())
                    .spawn(move || serve(stream, &inner, &token));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_TICK);
            }
            Err(_) => break,
        }
    }
}

fn serve(mut stream: UnixStream, inner: &Inner, token: &str) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(err) => {
            let _ = write_response(
                &mut stream,
                400,
                &serde_json::json!({"code": 2, "stdout": "", "stderr": format!("cap: {err}\n")}),
            );
            return;
        }
    };
    if request.token.as_deref() != Some(token) {
        let _ = write_response(
            &mut stream,
            401,
            &serde_json::json!({"code": 4, "stdout": "", "stderr": "cap: unauthorized\n"}),
        );
        return;
    }
    let call = match serde_json::from_slice::<Call>(&request.body) {
        Ok(call) => call,
        Err(err) => {
            let _ = write_response(
                &mut stream,
                400,
                &serde_json::json!({"code": 2, "stdout": "", "stderr": format!("cap: {err}\n")}),
            );
            return;
        }
    };
    let answer = answer(inner, &request.path, &call);
    let _ = write_response(&mut stream, 200, &answer);
}

fn answer(inner: &Inner, path: &str, call: &Call) -> serde_json::Value {
    let handlers = inner
        .handlers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match path {
        "/v1/search" => {
            let Some(chain) = handlers.search.clone() else {
                return unavailable("search");
            };
            drop(handlers);
            let outcome = crate::search_wire::execute_argv(&call.argv, &chain);
            serde_json::json!({
                "code": outcome.code,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
            })
        }
        "/v1/decide" => {
            let Some(setup) = handlers.decision.clone() else {
                return unavailable("decide");
            };
            drop(handlers);
            let outcome = crate::decision_wire::execute_argv(
                &call.argv,
                &call.stdin,
                &setup.instance,
                setup.key.as_deref(),
            );
            serde_json::json!({
                "code": outcome.code,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
            })
        }
        _ => serde_json::json!({"code": 2, "stdout": "", "stderr": "cap: unknown path\n"}),
    }
}

fn unavailable(name: &str) -> serde_json::Value {
    serde_json::json!({
        "code": 4,
        "stdout": "",
        "stderr": format!("{name} is not available in this session\n"),
    })
}

#[derive(serde::Deserialize, Default)]
struct Call {
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    stdin: String,
}

struct Request {
    path: String,
    token: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| err.to_string())?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(end) = find_header_end(&buffer) {
            break end;
        }
        if buffer.len() > 64 * 1024 {
            return Err("request headers are too large".into());
        }
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("the request ended before its headers".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or("no request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("no method")?;
    let path = parts.next().ok_or("no path")?.to_owned();
    if method != "POST" {
        return Err("only POST is accepted".into());
    }
    let mut token = None;
    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            token = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
                .map(str::to_owned);
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().map_err(|_| "bad content-length")?;
        }
    }
    if length > MAX_REQUEST_BYTES {
        return Err("the request body is too large".into());
    }
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("the request body ended early".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_BYTES + 64 * 1024 {
            return Err("the request body is too large".into());
        }
    }
    Ok(Request {
        path,
        token,
        body: buffer[body_start..body_start + length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_response(
    stream: &mut UnixStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// A per-session bearer token: 32 random bytes, hex.
fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The remote socket path for an SSH session. Short enough for the
/// kernel's 108-byte Unix-socket limit.
fn remote_socket(runtime_key: &str) -> String {
    let short: String = runtime_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(72)
        .collect();
    format!("/tmp/rhema-cap-{short}.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_websearch::mock::MockSearchProvider;

    fn call(endpoint: &Endpoint, path: &str, body: &str) -> serde_json::Value {
        let mut stream = UnixStream::connect(&endpoint.socket).expect("the endpoint listens");
        let request = format!(
            "POST {path} HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            endpoint.token,
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        let text = String::from_utf8(reply).unwrap();
        let (_, body) = text
            .split_once("\r\n\r\n")
            .expect("the response has a body");
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn a_search_request_round_trips_through_the_host_endpoint() {
        let winner = MockSearchProvider::new("brave").shared();
        winner.push_ok(vec![rhema_websearch::SearchResult::new(
            "T",
            "https://x",
            "snippet",
        )]);
        let mut chain = rhema_websearch::Chain::new();
        chain.push("brave-main", winner);
        let endpoint = wire("cap-test-search", None, Block::Search(Arc::new(chain)))
            .expect("a local endpoint starts");
        let reply = call(&endpoint, "/v1/search", r#"{"argv":["rust"]}"#);
        assert_eq!(reply["code"], 0, "{reply}");
        assert!(reply["stdout"].as_str().unwrap().contains("https://x"));
    }

    #[test]
    fn the_search_shell_body_reaches_the_endpoint() {
        let winner = MockSearchProvider::new("brave").shared();
        winner.push_ok(vec![rhema_websearch::SearchResult::new(
            "T",
            "https://x",
            "snippet",
        )]);
        let mut chain = rhema_websearch::Chain::new();
        chain.push("brave-main", winner);
        let endpoint = wire("cap-test-shell", None, Block::Search(Arc::new(chain)))
            .expect("a local endpoint starts");
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.bash");
        std::fs::write(
            &init,
            format!("search() {{\n{}\n}}\n", rhema_bash::websearch::SHELL_BODY),
        )
        .unwrap();
        let output = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                &format!("source {} && search rust", init.display()),
            ])
            .env("RHEMA_CAP_SOCKET", &endpoint.socket)
            .env("RHEMA_CAP_TOKEN", &endpoint.token)
            .output()
            .expect("bash runs");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("https://x"),
            "stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn the_decide_shell_body_sends_argv_and_reads_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cap.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).into_owned();
                    let length = header
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.contains(r#""argv":["noul","state","q"]"#), "{text}");
            let body = r#"{"code":0,"stdout":"noul: 0.90 yes\n","stderr":""}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let init = dir.path().join("init.bash");
        std::fs::write(
            &init,
            format!("decide() {{\n{}\n}}\n", rhema_bash::decision::SHELL_BODY),
        )
        .unwrap();
        let output = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                &format!("source {} && decide noul state q", init.display()),
            ])
            .env("RHEMA_CAP_SOCKET", &socket)
            .env("RHEMA_CAP_TOKEN", "t")
            .output()
            .expect("bash runs");
        server.join().unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "noul: 0.90 yes\n");
    }

    #[test]
    fn a_missing_token_is_refused() {
        let mut chain = rhema_websearch::Chain::new();
        chain.push("brave-main", MockSearchProvider::new("brave").shared());
        let endpoint = wire("cap-test-token", None, Block::Search(Arc::new(chain)))
            .expect("a local endpoint starts");
        let mut stream = UnixStream::connect(&endpoint.socket).unwrap();
        let body = r#"{"argv":["rust"]}"#;
        let request = format!(
            "POST /v1/search HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 401"));
    }
}
