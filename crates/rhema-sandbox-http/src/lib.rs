//! The HTTP sandbox client: the wire protocol over plain HTTPS.
//!
//! [`HttpSandbox`] is the same request/reply handling `SandboxDo`
//! performs over its DO binding, minus the binding: one JSON POST per
//! route to `{base}/{session}{route}`, the shared secret in
//! [`protocol::TOKEN_HEADER`](rhema_sandbox::protocol::TOKEN_HEADER). `base` is
//! the daemon's location (`https://box.example:7600`, no trailing
//! slash); the session name is the one path segment ahead of every
//! route — the daemon multiplexes one root directory per session name
//! (spec §3). The [`HttpTransport`] seam keeps reqwest out of the wasm
//! frontends: the native transport hides behind
//! [`HttpSandbox::new`], and any host that cannot own sockets brings
//! its own transport through `new_with_transport` — the
//! `AnthropicProvider` precedent. reqwest is a native-only dependency,
//! absent from the wasm build even with the feature on.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

#[cfg(not(target_arch = "wasm32"))]
use rhema_sandbox::protocol::TOKEN_HEADER;
use rhema_sandbox::protocol::{
    self, Ack, EXEC, ExecCall, ExecReply, FileReply, GET, PUT, PutFile, SEED, SeedRequest,
};
use rhema_sandbox::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// One JSON POST in, the status and body bytes out: the transport seam
/// under every route. The worker crate implements this over host fetch
/// (`rhema-cloudflare-code`); `HttpSandbox` stays transport-blind.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Posts `body` (JSON) to `url` with `token` in [`TOKEN_HEADER`],
    /// answering the response's status code and body bytes. Transport
    /// failures — unreachable, refused, cut short — are
    /// [`SandboxError::Unavailable`]; the status rides through
    /// unjudged, because only the route's caller knows which codes its
    /// reply shapes survive.
    async fn post(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), SandboxError>;
}

/// A `reqwest::Client` speaking the [`HttpTransport`] seam — the
/// transport behind [`HttpSandbox::new`], and the reason the `client`
/// feature carries reqwest. Native-only: reqwest's own wasm client is
/// `!Send` under this seam's `Send` contract, so a wasm build compiles
/// the rest of the module and brings its own transport instead.
#[cfg(not(target_arch = "wasm32"))]
struct ReqwestTransport {
    http: reqwest::Client,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReqwestTransport {
    /// A client with the total budget the longest route can want: an
    /// exec's real budget rides `timeout_secs` in the request body and
    /// the daemon kills the command itself, so the client's own
    /// ceiling only has to sit above any legal request.
    fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("a client builder with only timeouts cannot fail");
        Self { http }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), SandboxError> {
        let response = self
            .http
            .post(url)
            .header(TOKEN_HEADER, token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| SandboxError::Unavailable(err.to_string()))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|err| SandboxError::Unavailable(err.to_string()))?;
        Ok((status, bytes.to_vec()))
    }
}

/// The sandbox a session's commands run in, over plain HTTPS to a
/// daemon that speaks the protocol — `rhema-sandboxd` on some box, or
/// anything else serving the same routes.
pub struct HttpSandbox {
    /// The daemon's base URL (`https://box.example:7600`), no trailing
    /// slash: every request lands on `{base}/{session}{route}`.
    base: String,
    /// The session name — the one path segment between the base and
    /// the route, the daemon's multiplexing key.
    session: String,
    /// The shared secret checked on every route.
    token: String,
    transport: Arc<dyn HttpTransport>,
}

impl HttpSandbox {
    /// A client for the daemon at `base`, posting every route under
    /// the `session` path segment. The reqwest transport is
    /// native-only: wasm frontends bring their own [`HttpTransport`]
    /// through [`Self::new_with_transport`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(base: String, session: &str, token: &str) -> Self {
        Self::new_with_transport(base, session, token, Arc::new(ReqwestTransport::new()))
    }

    /// A client over a caller-supplied transport — the wasm frontend's
    /// seam, and any test's loopback double.
    pub fn new_with_transport(
        base: String,
        session: &str,
        token: &str,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            base,
            session: session.to_owned(),
            token: token.to_owned(),
            transport,
        }
    }

    /// One JSON POST on the session's route: the URL, the token
    /// header, the non-2xx gate, and the reply decode — the whole
    /// exchange the protocol fixes, done once so every route fails
    /// alike (`SandboxDo`'s wording, so the two clients fail alike).
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        body: &impl serde::Serialize,
    ) -> Result<T, SandboxError> {
        let url = format!("{}/{session}{route}", self.base, session = self.session);
        let payload = serde_json::to_vec(body).map_err(|err| {
            SandboxError::Unavailable(format!("request does not serialize: {err}"))
        })?;
        let (status, body) = self.transport.post(&url, &self.token, payload).await?;
        if !(200..300).contains(&status) {
            return Err(SandboxError::Unavailable(format!(
                "the sandbox daemon answered (HTTP {status}): {}",
                String::from_utf8_lossy(&body),
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|err| SandboxError::Unavailable(format!("malformed daemon reply: {err}")))
    }
}

/// The ack's `error` string as a sandbox result: absent and null both
/// mean success.
fn ack_result(ack: Ack) -> Result<(), SandboxError> {
    match ack.error {
        Some(message) => Err(SandboxError::Failed(message)),
        None => Ok(()),
    }
}

#[async_trait]
impl Sandbox for HttpSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        // Checked before the command starts, and nowhere else: a
        // command that completed remotely reports its true result —
        // the model sees the truth, whatever the turn decided
        // meanwhile.
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let call = ExecCall {
            command: request.command,
            cwd: Some(request.cwd),
            timeout_secs: request.timeout.as_secs(),
        };
        let reply: ExecReply = self.post(EXEC, &call).await?;
        if reply.timed_out {
            return Err(SandboxError::Timeout(request.timeout.as_secs()));
        }
        protocol::stream_reply(&reply, progress);
        Ok(ExecOutcome {
            stdout: reply.stdout.into_bytes(),
            stderr: reply.stderr.into_bytes(),
            code: reply.exit_code,
        })
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        let body = match seed {
            WorkspaceSeed::GitClone(url) => SeedRequest::Git { git: url.clone() },
            WorkspaceSeed::Tarball {
                name,
                tar_gz_base64,
            } => SeedRequest::Tarball {
                tarball_base64: tar_gz_base64.clone(),
                tarball_name: Some(name.clone()),
            },
        };
        let ack: Ack = self.post(SEED, &body).await?;
        ack_result(ack)
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        let reply: FileReply = self.post(GET, &serde_json::json!({ "path": path })).await?;
        if let Some(error) = reply.error {
            return Err(SandboxError::Failed(error));
        }
        base64::engine::general_purpose::STANDARD
            .decode(reply.content_base64.as_deref().unwrap_or_default())
            .map_err(|err| SandboxError::Failed(format!("malformed file reply: {err}")))
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        let put = PutFile {
            path: path.to_owned(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(contents),
        };
        let ack: Ack = self.post(PUT, &put).await?;
        ack_result(ack)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// One request the fake daemon saw: the path it routed on, the
    /// token header's value, and the decoded JSON body.
    struct Seen {
        path: String,
        token: Option<String>,
        body: serde_json::Value,
    }

    /// Serves `handler`'s canned answers on an ephemeral loopback
    /// port, one hand-rolled HTTP/1.1 exchange per connection — no
    /// axum, so the test pins the wire the daemon must speak, not a
    /// framework's reading of it. Answers the base URL and every
    /// request as the daemon saw it.
    async fn serve(
        handler: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, mpsc::Receiver<Seen>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback port binds");
        let addr = listener.local_addr().expect("the address reads");
        let (tx, rx) = mpsc::channel(8);
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let Ok(request) = read_request(&mut stream).await else {
                        return;
                    };
                    let (status, body) = handler(&request);
                    let _ = tx.send(request).await;
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        reason(status),
                        body.len(),
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// Reads one HTTP/1.1 request off the connection: the head to the
    /// blank line, then exactly Content-Length body bytes — just
    /// enough to route, which is all a protocol test should parse.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<Seen> {
        let mut raw: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let head_end = loop {
            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos + 4;
            }
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the connection closed before the head ended",
                ));
            }
            raw.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]);
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let mut token = None;
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "x-rhema-token" => token = Some(value.trim().to_owned()),
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        let mut body = raw[head_end..].to_vec();
        while body.len() < content_length {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);
        Ok(Seen {
            path,
            token,
            body: serde_json::from_slice(&body).unwrap_or_default(),
        })
    }

    /// The reason phrase the status line needs; the client reads the
    /// code, the phrase is for whoever reads a raw transcript.
    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Unknown",
        }
    }

    /// A sandbox aimed at the fake daemon, session `sess-7`.
    fn sandbox(base: String) -> HttpSandbox {
        HttpSandbox::new(base, "sess-7", "sekrit")
    }

    /// One command, budgeted for the test.
    fn exec_request(command: &str) -> ExecRequest {
        ExecRequest {
            command: command.to_owned(),
            cwd: "/workspace".into(),
            timeout: Duration::from_secs(120),
        }
    }

    #[tokio::test]
    async fn exec_posts_one_json_call_and_streams_the_reply() {
        let (base, mut requests) = serve(|seen| match seen.path.as_str() {
            "/sess-7/exec" => (
                200,
                json!({"exit_code": 0, "stdout": "hi\n", "stderr": "", "timed_out": false})
                    .to_string(),
            ),
            _ => (404, String::new()),
        })
        .await;
        let cancelled = AtomicBool::new(false);
        let mut lines: Vec<String> = Vec::new();
        let outcome = sandbox(base)
            .exec(exec_request("cargo test"), &cancelled, &mut |line| {
                lines.push(line.to_owned())
            })
            .await
            .expect("the exec answers");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");
        assert_eq!(outcome.stderr, b"");
        assert_eq!(lines, vec!["hi\n"]);
        let seen = requests.recv().await.expect("the request arrives");
        assert_eq!(seen.path, "/sess-7/exec");
        assert_eq!(seen.token.as_deref(), Some("sekrit"));
        assert_eq!(
            seen.body,
            json!({"command": "cargo test", "cwd": "/workspace", "timeout_secs": 120}),
        );
    }

    #[tokio::test]
    async fn a_timed_out_reply_maps_to_the_timeout_error() {
        let (base, _requests) = serve(|_| {
            (
                200,
                json!({"exit_code": 137, "stdout": "partial\n", "stderr": "", "timed_out": true})
                    .to_string(),
            )
        })
        .await;
        let cancelled = AtomicBool::new(false);
        let error = sandbox(base)
            .exec(exec_request("sleep 100"), &cancelled, &mut |_| {})
            .await
            .expect_err("the kill surfaces");
        assert_eq!(error, SandboxError::Timeout(120));
    }

    #[tokio::test]
    async fn a_cancelled_exec_never_reaches_the_wire() {
        let (base, mut requests) = serve(|_| (200, String::new())).await;
        let cancelled = AtomicBool::new(true);
        let error = sandbox(base)
            .exec(exec_request("true"), &cancelled, &mut |_| {})
            .await
            .expect_err("a cancelled run does not start");
        assert_eq!(error, SandboxError::Failed("cancelled".into()));
        assert!(requests.try_recv().is_err(), "nothing was posted");
    }

    #[tokio::test]
    async fn seed_maps_both_workspace_shapes_onto_the_seed_route() {
        let (base, mut requests) = serve(|seen| match seen.path.as_str() {
            "/sess-7/seed" => (200, json!({"error": null}).to_string()),
            _ => (404, String::new()),
        })
        .await;
        let sbx = sandbox(base);
        sbx.seed(&WorkspaceSeed::GitClone(
            "https://github.com/example/repo".into(),
        ))
        .await
        .expect("the git seed lands");
        sbx.seed(&WorkspaceSeed::Tarball {
            name: "repo.tar.gz".into(),
            tar_gz_base64: "aGk=".into(),
        })
        .await
        .expect("the tarball seed lands");
        let git = requests.recv().await.expect("the git seed arrives");
        assert_eq!(git.body, json!({"git": "https://github.com/example/repo"}));
        let tarball = requests.recv().await.expect("the tarball seed arrives");
        assert_eq!(
            tarball.body,
            json!({"tarball_base64": "aGk=", "tarball_name": "repo.tar.gz"}),
        );
    }

    #[tokio::test]
    async fn read_file_decodes_the_file_reply() {
        let (base, mut requests) = serve(|seen| match seen.path.as_str() {
            "/sess-7/get" => (
                200,
                json!({"content_base64": "aGk=", "error": null}).to_string(),
            ),
            _ => (404, String::new()),
        })
        .await;
        let contents = sandbox(base)
            .read_file("/workspace/SYSTEM.md")
            .await
            .expect("the file reads");
        assert_eq!(contents, b"hi");
        let seen = requests.recv().await.expect("the request arrives");
        assert_eq!(seen.path, "/sess-7/get");
        assert_eq!(seen.body, json!({"path": "/workspace/SYSTEM.md"}));
    }

    #[tokio::test]
    async fn a_failed_file_reply_is_a_failed_error() {
        let (base, _requests) = serve(|_| {
            (
                200,
                json!({"content_base64": null, "error": "no such file"}).to_string(),
            )
        })
        .await;
        let error = sandbox(base)
            .read_file("/workspace/missing")
            .await
            .expect_err("the failure surfaces");
        assert_eq!(error, SandboxError::Failed("no such file".into()));
    }

    #[tokio::test]
    async fn write_file_posts_a_put() {
        let (base, mut requests) = serve(|seen| match seen.path.as_str() {
            "/sess-7/put" => (200, json!({"error": null}).to_string()),
            _ => (404, String::new()),
        })
        .await;
        sandbox(base)
            .write_file("/workspace/SYSTEM.md", b"hi")
            .await
            .expect("the write lands");
        let seen = requests.recv().await.expect("the request arrives");
        assert_eq!(seen.path, "/sess-7/put");
        assert_eq!(seen.body["path"], "/workspace/SYSTEM.md");
        assert_eq!(seen.body["content_base64"], "aGk=");
    }

    #[tokio::test]
    async fn a_refused_token_fails_alike_with_the_other_client() {
        let (base, _requests) = serve(|_| (401, "wrong token".to_owned())).await;
        let cancelled = AtomicBool::new(false);
        let error = sandbox(base)
            .exec(exec_request("true"), &cancelled, &mut |_| {})
            .await
            .expect_err("the refusal surfaces");
        match error {
            SandboxError::Unavailable(message) => assert!(
                message.contains("the sandbox daemon answered (HTTP 401)")
                    && message.contains("wrong token"),
                "the message carries the status and the body: {message}",
            ),
            other => panic!("a refused token is unavailable, not {other:?}"),
        }
    }
}
