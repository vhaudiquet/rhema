//! One connection over one transport: the handshake, the pending map,
//! per-request timeouts, cancellation, and the Streamable HTTP session
//! dance.
//!
//! Stdio: the child's stdout is newline-delimited JSON-RPC read under
//! a hard line cap — a garbage line or an oversized one kills the
//! connection, never this process. The child's stderr drains into an
//! 8 KiB ring so a death carries its own last words. Requests
//! multiplex freely; answers route by id.
//!
//! HTTP: POST-only Streamable HTTP. The session id captured from the
//! handshake rides every later request with the negotiated
//! `MCP-Protocol-Version`; a `404` under a session id means the
//! session expired and triggers exactly one re-initialize-and-retry.
//! Calls serialize on the session lock: this client's call volume is
//! a shell command at a time, and the session state must stay
//! coherent above all.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::BodyExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::protocol::{
    self, CallToolResult, Incoming, RequestId, RpcError, RpcNotification, RpcRequest, ToolDef,
    method,
};

/// The hard cap on one stdio stdout line: a server that cannot frame
/// is a server that cannot be trusted with this process's memory.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The stderr ring size kept for a stdio server's death report.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Why a connection died.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The stdio child's transport failed at the OS level.
    #[error("stdio transport failed: {0}")]
    Stdio(std::io::Error),
    /// The HTTP transport failed below the protocol.
    #[error("http transport failed: {0}")]
    Http(String),
    /// The server broke the framing rules (a garbage or oversized
    /// line, a malformed envelope, a response lost from an SSE
    /// stream).
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    /// The server answered the handshake with a revision outside the
    /// transport's accept-set.
    #[error(
        "server offered protocol version {offered}, which the {transport} \
         transport does not support (accepts {accepted})"
    )]
    VersionMismatch {
        /// What the server answered.
        offered: String,
        /// Which transport's accept-set refused it.
        transport: &'static str,
        /// The accept-set, for whoever reads the error.
        accepted: String,
    },
    /// The stdio child exited; the tail is its last stderr, capped.
    #[error("server exited: {stderr_tail}")]
    ServerExited {
        /// The server's final stderr, up to [`STDERR_TAIL_BYTES`].
        stderr_tail: String,
    },
    /// The server answered a request with a JSON-RPC error.
    #[error("server error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The server's message.
        message: String,
    },
    /// The HTTP server answered with an unexpected status.
    #[error("http status {0}")]
    HttpStatus(u16),
    /// The request outlived its budget.
    #[error("request timed out")]
    Timeout,
    /// The caller abandoned the request.
    #[error("cancelled")]
    Cancelled,
    /// The connection died of an earlier failure; nothing more can be
    /// sent on it.
    #[error("{0}")]
    Dead(#[from] DeadConnection),
}

/// The carried reason a connection is unusable.
#[derive(Debug, thiserror::Error)]
pub enum DeadConnection {
    /// The framing rules were broken.
    #[error("connection killed after a protocol violation")]
    Violation,
    /// The stdio child exited.
    #[error("the server process exited")]
    Exited,
}

/// Which transport a connection speaks, and therefore which revision
/// accept-set applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A spawned local child on stdio.
    Stdio,
    /// A Streamable HTTP endpoint.
    Http,
}

impl Transport {
    /// The revisions this transport accepts, per spec §6.
    #[must_use]
    pub fn accepted_versions(self) -> &'static [&'static str] {
        match self {
            Self::Stdio => &protocol::SUPPORTED_STDIO,
            Self::Http => &protocol::SUPPORTED_HTTP,
        }
    }

    /// Whether an offered handshake revision is acceptable.
    #[must_use]
    pub fn accepts(self, offered: &str) -> bool {
        self.accepted_versions().contains(&offered)
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio => f.write_str("stdio"),
            Self::Http => f.write_str("http"),
        }
    }
}

/// Parameters for a stdio connection.
#[derive(Clone)]
pub struct StdioParams {
    /// The program to spawn.
    pub program: String,
    /// The program's arguments.
    pub args: Vec<String>,
    /// Extra environment layered over the inherited environment.
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for StdioParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdioParams")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &SecretMap(&self.env))
            .finish()
    }
}

/// Parameters for a Streamable HTTP connection.
#[derive(Clone)]
pub struct HttpParams {
    /// The endpoint URL.
    pub url: String,
    /// Headers sent verbatim with every request.
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for HttpParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpParams")
            .field("url", &self.url)
            .field("headers", &SecretMap(&self.headers))
            .finish()
    }
}

/// A map whose values print as `[…]`: keys shown, values redacted.
struct SecretMap<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for SecretMap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: BTreeMap<&String, &'static str> =
            self.0.keys().map(|key| (key, "[…]")).collect();
        f.debug_map().entries(redacted).finish()
    }
}

/// A cancellation token: fire it to abandon in-flight requests that
/// carry it. The bridge layer fires one per disconnecting caller.
#[derive(Clone, Default)]
pub struct Cancel {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    /// A token nobody has fired.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires the token: every waiter returns.
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether the token has fired.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// Resolves once the token has fired, immediately if it has.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// The state every request goes through, shared with the transport
/// tasks.
struct Shared {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, ClientError>>>>,
    stderr_tail: Mutex<Vec<u8>>,
    dead: Mutex<Option<ClientError>>,
}

impl Shared {
    /// Marks the connection dead (first writer wins) and fails every
    /// pending request with the given reason.
    fn fail_all(&self, dead_reason: ClientError, pending_reason: PendingReason) {
        let mut dead = self.dead.lock().expect("dead lock");
        if dead.is_none() {
            *dead = Some(dead_reason);
        }
        drop(dead);
        let mut pending = self.pending.lock().expect("pending lock");
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(match &pending_reason {
                PendingReason::Protocol(text) => ClientError::Protocol(text),
                PendingReason::Exited { tail } => ClientError::ServerExited {
                    stderr_tail: tail.clone(),
                },
            }));
        }
    }

    /// Fails every pending request with a fresh `reason()` each; the
    /// dead mark is left alone.
    fn drain_pending_with(&self, reason: impl Fn() -> ClientError) {
        let mut pending = self.pending.lock().expect("pending lock");
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(reason()));
        }
    }

    /// The stderr ring's contents, lossily decoded.
    fn take_stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().expect("stderr lock");
        String::from_utf8_lossy(&tail).into_owned()
    }

    /// The reason this connection is dead, when it is.
    fn dead_error(&self) -> Option<ClientError> {
        self.dead
            .lock()
            .expect("dead lock")
            .as_ref()
            .map(|err| match err {
                ClientError::Dead(DeadConnection::Violation) => {
                    ClientError::Dead(DeadConnection::Violation)
                }
                ClientError::Dead(DeadConnection::Exited) => {
                    ClientError::Dead(DeadConnection::Exited)
                }
                ClientError::ServerExited { stderr_tail } => ClientError::ServerExited {
                    stderr_tail: stderr_tail.clone(),
                },
                ClientError::Protocol(text) => ClientError::Protocol(text),
                _ => ClientError::Dead(DeadConnection::Violation),
            })
    }
}

/// What the reader tells the death watcher.
enum KillSignal {
    /// A framing violation: terminate the child now.
    Violation,
    /// The child's stdout ended: it is exiting on its own, reap it
    /// and report the death with its stderr tail.
    StreamEnded,
}

/// Why pending requests are being failed.
enum PendingReason {
    /// Framing or envelope violation.
    Protocol(&'static str),
    /// The child exited, with its stderr tail.
    Exited { tail: String },
}

/// One live MCP connection over one transport.
pub struct Client {
    shared: Arc<Shared>,
    transport: Transport,
    /// stdio: the line sender to the writer task; dropped on kill.
    writer: Option<mpsc::UnboundedSender<String>>,
    http: Option<HttpState>,
}

/// The HTTP transport's state: the pooled hyper client, the endpoint,
/// config headers, and the negotiated session.
#[derive(Clone)]
struct HttpState {
    client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        http_body_util::Full<bytes::Bytes>,
    >,
    url: hyper::Uri,
    headers: BTreeMap<String, String>,
    session: Arc<tokio::sync::Mutex<HttpSession>>,
}

/// The session captured at handshake: its id, and the revision in
/// force, which rides `MCP-Protocol-Version` on every later request.
#[derive(Default)]
struct HttpSession {
    id: Option<String>,
    version: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("Client");
        out.field("transport", &self.transport);
        if let Some(http) = &self.http {
            out.field("url", &http.url);
        }
        let dead = self.shared.dead.lock().expect("dead lock").is_some();
        out.field("dead", &dead).finish()
    }
}

impl Client {
    /// Connects over stdio: spawns the child, handshakes under
    /// `startup_timeout`, and leaves the reader watching the child's
    /// stdout.
    ///
    /// # Errors
    ///
    /// Spawn failure, I/O failure, handshake timeout, or a handshake
    /// answered with a revision outside the stdio accept-set.
    pub async fn connect_stdio(
        params: StdioParams,
        startup_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let mut command = tokio::process::Command::new(&params.program);
        command
            .args(&params.args)
            .envs(&params.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(ClientError::Stdio)?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let shared = Arc::new(Shared {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stderr_tail: Mutex::new(Vec::new()),
            dead: Mutex::new(None),
        });

        // stderr → the ring buffer, until the child exits.
        let tail_shared = Arc::clone(&shared);
        tokio::spawn(drain_stderr(stderr, tail_shared));

        // Writer: owns the child's stdin, takes lines from the
        // channel; the channel closing closes stdin.
        let (writer, writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(writer_task(stdin, writer_rx));

        // Reader: routes lines to the pending map; signals the death
        // watcher on EOF or framing violation. The signal carries
        // which: a violation terminates the child, a stream end means
        // the child is already going.
        let (kill_tx, kill_rx) = oneshot::channel::<KillSignal>();
        let reader_shared = Arc::clone(&shared);
        tokio::spawn(reader_task(
            tokio::io::BufReader::new(stdout),
            reader_shared,
            kill_tx,
        ));

        // Death watcher: on child exit, fails pending with the
        // stderr tail; on kill signal, terminates the child.
        let mut child_for_wait = child;
        let wait_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            tokio::select! {
                status = child_for_wait.wait() => {
                    let tail = wait_shared.take_stderr_tail();
                    let _ = status;
                    wait_shared.fail_all(
                        ClientError::Dead(DeadConnection::Exited),
                        PendingReason::Exited { tail },
                    );
                }
                signal = kill_rx => match signal {
                    Ok(KillSignal::Violation) => {
                        let _ = child_for_wait.start_kill();
                        let _ = child_for_wait.wait().await;
                        // The reader marked the connection dead
                        // before signaling; anything registered
                        // since drains with that same reason.
                        wait_shared.drain_pending_with(|| {
                            wait_shared
                                .dead_error()
                                .unwrap_or(ClientError::Dead(DeadConnection::Violation))
                        });
                    }
                    Ok(KillSignal::StreamEnded) => {
                        // Reap the exiting child and report the death
                        // with its tail; the wait above may also have
                        // resolved, in which case this is a no-op.
                        let _ = child_for_wait.wait().await;
                        let tail = wait_shared.take_stderr_tail();
                        wait_shared.fail_all(
                            ClientError::Dead(DeadConnection::Exited),
                            PendingReason::Exited { tail },
                        );
                    }
                    Err(_closed) => {}
                },
            }
        });

        let client = Self {
            shared,
            transport: Transport::Stdio,
            writer: Some(writer),
            http: None,
        };
        client.handshake(startup_timeout).await?;
        Ok(client)
    }

    /// Connects over Streamable HTTP: performs the handshake POSTs
    /// and stores the session.
    ///
    /// # Errors
    ///
    /// URL parse failure, transport failure, a failed handshake
    /// exchange, or an unsupported revision.
    pub async fn connect_http(params: HttpParams) -> Result<Self, ClientError> {
        let url: hyper::Uri = params
            .url
            .parse()
            .map_err(|err| ClientError::Http(format!("endpoint url: {err}")))?;
        let client: hyper_util::client::legacy::Client<
            hyper_util::client::legacy::connect::HttpConnector,
            http_body_util::Full<bytes::Bytes>,
        > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
        let state = HttpState {
            client,
            url,
            headers: params.headers,
            session: Arc::new(tokio::sync::Mutex::new(HttpSession::default())),
        };
        let shared = Arc::new(Shared {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stderr_tail: Mutex::new(Vec::new()),
            dead: Mutex::new(None),
        });
        let me = Self {
            shared,
            transport: Transport::Http,
            writer: None,
            http: Some(state),
        };
        me.handshake_http().await?;
        Ok(me)
    }

    /// Which transport this connection speaks.
    #[must_use]
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// The stdio server's last stderr, capped at
    /// [`STDERR_TAIL_BYTES`]; empty until it says something.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.shared.take_stderr_tail()
    }

    /// Whether the connection died of an earlier failure.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.shared.dead.lock().expect("dead lock").is_some()
    }

    /// Lists every tool, following pagination cursors to the end.
    ///
    /// # Errors
    ///
    /// Transport failure, protocol violation, timeout, cancellation,
    /// or an RPC error from the server.
    pub async fn tools_list(
        &self,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Vec<ToolDef>, ClientError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        loop {
            let params = serde_json::to_value(protocol::ToolsListParams {
                cursor: cursor.clone(),
            })
            .expect("tools/list params serialize");
            let result = self
                .request(method::TOOLS_LIST, Some(params), timeout, cancel)
                .await?;
            let page: protocol::ToolsListResult = serde_json::from_value(result)
                .map_err(|_| ClientError::Protocol("tools/list result malformed"))?;
            tools.extend(page.tools);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(tools),
            }
        }
    }

    /// Invokes a tool.
    ///
    /// `is_error` on the result is data, not this error type: a tool
    /// that ran and reported failure is a successful round trip, and
    /// the exit-code story is the CLI layer's decision.
    ///
    /// # Errors
    ///
    /// Transport failure, protocol violation, timeout, cancellation,
    /// or an RPC error from the server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<CallToolResult, ClientError> {
        let params = serde_json::to_value(protocol::ToolsCallParams {
            name: name.to_owned(),
            arguments,
        })
        .expect("tools/call params serialize");
        let result = self
            .request(method::TOOLS_CALL, Some(params), timeout, cancel)
            .await?;
        serde_json::from_value(result).map_err(|_| ClientError::Protocol("call result malformed"))
    }

    /// Sends one request and awaits its answer under `timeout`,
    /// abandonable through `cancel`.
    ///
    /// # Errors
    ///
    /// Everything [`ClientError`] covers except the internal variants.
    async fn request(
        &self,
        method_name: &'static str,
        params: Option<Value>,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Value, ClientError> {
        if let Some(err) = self.shared.dead_error() {
            return Err(err);
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let line = RpcRequest::new(id, method_name, params).to_line();

        match self.transport {
            Transport::Stdio => {
                // Register before sending: a fast server must not find
                // the pending map empty.
                let (tx, rx) = oneshot::channel();
                self.shared
                    .pending
                    .lock()
                    .expect("pending lock")
                    .insert(id, tx);
                let sent = self
                    .writer
                    .as_ref()
                    .expect("writer on stdio transport")
                    .send(line);
                if sent.is_err() {
                    // The writer is gone: the child died without the
                    // reader noticing yet.
                    self.shared
                        .pending
                        .lock()
                        .expect("pending lock")
                        .remove(&id);
                    return Err(self
                        .shared
                        .dead_error()
                        .unwrap_or(ClientError::Dead(DeadConnection::Exited)));
                }
                self.await_answer(id, rx, timeout, cancel).await
            }
            Transport::Http => {
                let http = self.http.as_ref().expect("http state on http transport");
                let fut = self.http_exchange(http, id, &line);
                tokio::pin!(fut);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.abandon_http(id).await;
                        Err(ClientError::Cancelled)
                    }
                    answer = tokio::time::timeout(timeout, &mut fut) => match answer {
                        Ok(result) => result,
                        Err(_elapsed) => {
                            self.abandon_http(id).await;
                            Err(ClientError::Timeout)
                        }
                    },
                }
            }
        }
    }

    /// Waits for a stdio request's answer: response, timeout, cancel,
    /// or death.
    async fn await_answer(
        &self,
        id: u64,
        rx: oneshot::Receiver<Result<Value, ClientError>>,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Value, ClientError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.abandon_stdio(id);
                Err(ClientError::Cancelled)
            }
            answer = tokio::time::timeout(timeout, rx) => match answer {
                Ok(Ok(value)) => value,
                Ok(Err(_dropped)) => Err(self.shared.dead_error().unwrap_or({
                    ClientError::Dead(DeadConnection::Exited)
                })),
                Err(_elapsed) => {
                    self.abandon_stdio(id);
                    Err(ClientError::Timeout)
                }
            },
        }
    }

    /// Drops a pending stdio entry and tells the server over the
    /// wire, unconditionally and best-effort: cancellation needs no
    /// capability (spec §6).
    fn abandon_stdio(&self, id: u64) {
        if self.take_pending(id).is_some() {
            let line = cancelled_line(id);
            if let Some(writer) = &self.writer {
                let _ = writer.send(line);
            }
        }
    }

    /// Same for HTTP: the notification POSTs from a detached task, so
    /// a cancelled caller is never blocked on it.
    async fn abandon_http(&self, id: u64) {
        if self.take_pending(id).is_some() {
            let http = self.http.clone().expect("http state on http transport");
            let line = cancelled_line(id);
            tokio::spawn(async move {
                let _ = http.post_line(&line).await;
            });
        }
    }

    /// Removes a pending entry; `Some` means this caller owned it.
    fn take_pending(&self, id: u64) -> Option<()> {
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .remove(&id)
            .map(|_| ())
    }

    /// The handshake, stdio flavor: initialize, check the accept-set,
    /// notify initialized.
    async fn handshake(&self, startup_timeout: Duration) -> Result<(), ClientError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let line =
            RpcRequest::new(id, method::INITIALIZE, Some(protocol::initialize_params())).to_line();
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id, tx);
        let sent = self
            .writer
            .as_ref()
            .expect("writer on stdio transport")
            .send(line);
        if sent.is_err() {
            return Err(self
                .shared
                .dead_error()
                .unwrap_or(ClientError::Dead(DeadConnection::Exited)));
        }
        let value = match tokio::time::timeout(startup_timeout, rx).await {
            Ok(Ok(answer)) => answer?,
            Ok(Err(_dropped)) => Err(self
                .shared
                .dead_error()
                .unwrap_or(ClientError::Dead(DeadConnection::Exited)))?,
            Err(_elapsed) => return Err(ClientError::Timeout),
        };
        let init: protocol::InitializeResult = serde_json::from_value(value)
            .map_err(|_| ClientError::Protocol("initialize result malformed"))?;
        check_version(self.transport, &init.protocol_version)?;
        if let Some(writer) = &self.writer {
            let _ = writer.send(RpcNotification::new(method::INITIALIZED, None).to_line());
        }
        Ok(())
    }

    /// The handshake, HTTP flavor: initialize POST, session capture,
    /// the notification POST.
    async fn handshake_http(&self) -> Result<(), ClientError> {
        let http = self.http.as_ref().expect("http state on http transport");
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let line =
            RpcRequest::new(id, method::INITIALIZE, Some(protocol::initialize_params())).to_line();
        let (status, headers, body) = http.post_line(&line).await?;
        if !status.is_success() {
            return Err(ClientError::HttpStatus(status.as_u16()));
        }
        let body = collect_body(body).await?;
        let response = parse_json_response(&body)?;
        let value = envelope_value(response)?;
        let init: protocol::InitializeResult = serde_json::from_value(value)
            .map_err(|_| ClientError::Protocol("initialize result malformed"))?;
        check_version(self.transport, &init.protocol_version)?;

        let mut session = http.session.lock().await;
        session.id = headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        session.version = init.protocol_version;
        drop(session);

        let notification = RpcNotification::new(method::INITIALIZED, None).to_line();
        let (status, _headers, _body) = http.post_line(&notification).await?;
        // A notification POST reads as success when answered 2xx with
        // no body (spec §6: 202 Accepted).
        if !status.is_success() {
            return Err(ClientError::HttpStatus(status.as_u16()));
        }
        Ok(())
    }

    /// One HTTP request-response exchange, with the 404
    /// re-initialize-and-retry-once dance (spec §6).
    async fn http_exchange(
        &self,
        http: &HttpState,
        _id: u64,
        line: &str,
    ) -> Result<Value, ClientError> {
        for _attempt in 0..2 {
            let session = http.session.lock().await;
            let had_session = session.id.is_some();
            let (status, headers, body) = http.post_line_with(line, &session).await?;
            drop(session);
            if status.as_u16() == 404 && had_session {
                // Session expired: re-initialize exactly once, retry.
                self.handshake_http().await?;
                continue;
            }
            let content_type = headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if content_type.starts_with("text/event-stream") {
                return sse_answer(body, _id).await;
            }
            if !status.is_success() {
                return Err(ClientError::HttpStatus(status.as_u16()));
            }
            let body = collect_body(body).await?;
            if body.is_empty() {
                // 202 with no body means "response on another
                // channel"; this client opens no such channel.
                return Err(ClientError::Protocol(
                    "server accepted the request without answering it",
                ));
            }
            let response = parse_json_response(&body)?;
            return envelope_value(response);
        }
        Err(ClientError::Protocol(
            "re-initialization did not recover the session",
        ))
    }
}

/// Builds the `notifications/cancelled` wire line for a request id.
fn cancelled_line(id: u64) -> String {
    let params = serde_json::to_value(protocol::CancelledParams {
        request_id: RequestId::Number(id),
    })
    .expect("cancelled params serialize");
    RpcNotification::new(method::CANCELLED, Some(params)).to_line()
}

/// Drains a response body into its bytes.
async fn collect_body(body: hyper::body::Incoming) -> Result<bytes::Bytes, ClientError> {
    BodyExt::collect(body)
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|err| ClientError::Http(err.to_string()))
}

/// Parses a body as a JSON-RPC response envelope.
fn parse_json_response(body: &[u8]) -> Result<protocol::Response, ClientError> {
    let incoming: Incoming = serde_json::from_slice(body)
        .map_err(|_| ClientError::Protocol("response envelope malformed"))?;
    let Incoming::Response(response) = incoming else {
        return Err(ClientError::Protocol(
            "server answered a request with a notification",
        ));
    };
    Ok(response)
}

/// Extracts result-or-error from a response envelope.
fn envelope_value(response: protocol::Response) -> Result<Value, ClientError> {
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(RpcError { code, message })) => Err(ClientError::Rpc { code, message }),
        (None, None) => Err(ClientError::Protocol(
            "response carried neither result nor error",
        )),
        (Some(_), Some(_)) => Err(ClientError::Protocol(
            "response carried both result and error",
        )),
    }
}

/// Checks an offered handshake revision against the transport's
/// accept-set.
fn check_version(transport: Transport, offered: &str) -> Result<(), ClientError> {
    if transport.accepts(offered) {
        return Ok(());
    }
    Err(ClientError::VersionMismatch {
        offered: offered.to_owned(),
        transport: match transport {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        },
        accepted: transport.accepted_versions().join(", "),
    })
}

impl HttpState {
    /// One POST with the transport's fixed, session, and config
    /// headers. Takes the session lock itself (handshake path).
    async fn post_line(
        &self,
        line: &str,
    ) -> Result<(hyper::StatusCode, hyper::HeaderMap, hyper::body::Incoming), ClientError> {
        let session = self.session.lock().await;
        self.post_line_with(line, &session).await
    }

    /// The POST with an already-held session guard.
    async fn post_line_with(
        &self,
        line: &str,
        session: &HttpSession,
    ) -> Result<(hyper::StatusCode, hyper::HeaderMap, hyper::body::Incoming), ClientError> {
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(self.url.clone())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(id) = &session.id {
            builder = builder.header("mcp-session-id", id.clone());
        }
        if !session.version.is_empty() {
            builder = builder.header("mcp-protocol-version", session.version.clone());
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.clone(), value.clone());
        }
        let request = builder
            .body(http_body_util::Full::new(bytes::Bytes::from(
                line.to_owned(),
            )))
            .map_err(|err| ClientError::Http(format!("request build: {err}")))?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|err| ClientError::Http(err.to_string()))?;
        let (parts, body) = response.into_parts();
        Ok((parts.status, parts.headers, body))
    }
}

/// Reads stderr into the shared ring, keeping the last
/// [`STDERR_TAIL_BYTES`] bytes.
async fn drain_stderr(mut stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut tail = shared.stderr_tail.lock().expect("stderr lock");
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                if excess > 0 {
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// Owns the child's stdin and writes lines to it.
async fn writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        if stdin.write_all(b"\n").await.is_err() {
            return;
        }
        if stdin.flush().await.is_err() {
            return;
        }
    }
}

/// Routes stdout lines: responses to the pending map, violations and
/// EOF to the kill switch. Owns no child handles; killing is the
/// death watcher's job.
async fn reader_task(
    mut reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    shared: Arc<Shared>,
    kill_tx: oneshot::Sender<KillSignal>,
) {
    let die = |shared: &Shared, reason: PendingReason| {
        shared.fail_all(ClientError::Dead(DeadConnection::Violation), reason);
    };
    loop {
        match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
            Ok(None) => {
                // EOF: the child is exiting; the watcher reaps it and
                // reports the death with the stderr tail.
                let _ = kill_tx.send(KillSignal::StreamEnded);
                return;
            }
            Ok(Some(raw)) => {
                let line = strip_eol(&raw);
                if line.is_empty() {
                    continue;
                }
                let incoming = match serde_json::from_slice::<Incoming>(line) {
                    Ok(incoming) => incoming,
                    Err(_) => {
                        die(
                            &shared,
                            PendingReason::Protocol("a stdout line was not valid JSON-RPC"),
                        );
                        let _ = kill_tx.send(KillSignal::Violation);
                        return;
                    }
                };
                match incoming {
                    Incoming::Response(response) => route_response(&shared, response),
                    Incoming::Notification(notification) => {
                        route_notification(&shared, notification)
                    }
                }
            }
            Err(LineError::TooLong) => {
                die(
                    &shared,
                    PendingReason::Protocol("a stdout line exceeded the frame cap"),
                );
                let _ = kill_tx.send(KillSignal::Violation);
                return;
            }
            Err(LineError::Io) => {
                die(&shared, PendingReason::Protocol("the stdout pipe failed"));
                let _ = kill_tx.send(KillSignal::Violation);
                return;
            }
        }
    }
}

/// Sends a response to whoever waits on its id; unmatched responses
/// (late, or to an abandoned request) drop silently.
fn route_response(shared: &Shared, response: protocol::Response) {
    let RequestId::Number(id) = response.id else {
        return;
    };
    let sender = shared.pending.lock().expect("pending lock").remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(envelope_value(response));
    }
}

/// Routes server notifications: only `notifications/cancelled` means
/// anything to this subset; everything else (progress, log) is data
/// this layer does not surface.
fn route_notification(shared: &Shared, notification: protocol::Notification) {
    if notification.method != method::CANCELLED {
        return;
    }
    let Some(params) = notification.params else {
        return;
    };
    let Ok(cancelled) = serde_json::from_value::<protocol::CancelledParams>(params) else {
        return;
    };
    let RequestId::Number(id) = cancelled.request_id else {
        return;
    };
    if let Some(sender) = shared.pending.lock().expect("pending lock").remove(&id) {
        let _ = sender.send(Err(ClientError::Cancelled));
    }
}

/// Why a line read failed. The pipe's own error detail is dropped:
/// "the stdout pipe failed" is all a caller can act on.
enum LineError {
    /// The line outgrew the frame cap.
    TooLong,
    /// The pipe itself failed.
    Io,
}

/// Reads one `\n`-terminated line with a hard cap: the buffered
/// readers' own line methods grow without bound on a hostile stream.
/// A final line with no trailing newline still counts as a line.
async fn read_line_capped<R: tokio::io::AsyncBufRead + std::marker::Unpin>(
    reader: &mut R,
    cap: usize,
) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    loop {
        let (done, used) = match reader.fill_buf().await.map_err(|_| LineError::Io)? {
            // EOF with nothing buffered: a clean end of stream.
            [] => {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(line))
                };
            }
            buffered => match buffered.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    if line.len() + pos + 1 > cap {
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(&buffered[..=pos]);
                    (true, pos + 1)
                }
                None => {
                    if line.len() + buffered.len() > cap {
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(buffered);
                    (false, buffered.len())
                }
            },
        };
        reader.consume(used);
        if done {
            return Ok(Some(line));
        }
    }
}

/// Strips a trailing `\n` or `\r\n`.
fn strip_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// Waits for the request's answer inside an SSE stream: frames
/// accumulate, `data:` lines build events, and the event carrying the
/// request's id wins. Other events pass by.
async fn sse_answer(body: hyper::body::Incoming, id: u64) -> Result<Value, ClientError> {
    let mut body = body;
    let mut buffer: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    loop {
        let chunk = match body.frame().await {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(chunk) => chunk,
                // A trailer, not data: skip it.
                Err(_frame) => continue,
            },
            Some(Err(err)) => return Err(ClientError::Http(err.to_string())),
            None => {
                return Err(ClientError::Protocol(
                    "the SSE stream closed before the response arrived",
                ));
            }
        };
        buffer.extend_from_slice(&chunk);
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=pos).collect();
            line.pop(); // the newline
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                // Event boundary: dispatch what data lines gathered.
                if let Some(answer) = event_answer(&data_lines, id)? {
                    return Ok(answer);
                }
                data_lines.clear();
            } else if let Some(data) = line.strip_prefix("data:") {
                data_lines.push(data.trim_start().to_owned());
            }
            // Other SSE fields (`event:`, `id:`, `retry:`) pass by.
        }
    }
}

/// Joins an event's data lines and, when the event carries the wanted
/// response, returns its value. Notifications riding the stream, and
/// responses to other ids, pass by.
fn event_answer(data_lines: &[String], id: u64) -> Result<Option<Value>, ClientError> {
    if data_lines.is_empty() {
        return Ok(None);
    }
    let joined = data_lines.join("\n");
    let incoming: Incoming = serde_json::from_str(&joined)
        .map_err(|_| ClientError::Protocol("an SSE event was not valid JSON-RPC"))?;
    let Incoming::Response(response) = incoming else {
        return Ok(None);
    };
    match &response.id {
        RequestId::Number(n) if *n == id => envelope_value(response).map(Some),
        _ => Ok(None),
    }
}

/// Compile-time transport guarantees: a `Client` crosses tasks and is
/// shared across the manager's handles.
///
/// # Panics
///
/// Never; the function exists so the bounds are checked.
#[doc(hidden)]
pub fn assert_send_sync()
where
    Client: Send + Sync,
    Cancel: Send + Sync,
{
}
