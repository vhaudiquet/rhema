//! One connection over one transport: the handshake, the pending
//! map, per-request timeouts, and cancellation. The per-transport
//! machinery lives in the child modules: the spawned-child pipes in
//! `stdio`, the Streamable HTTP session dance in `http`.

mod http;
mod stdio;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};

use self::http::{HttpClient, HttpSession, HttpState};
use self::stdio::{KillSignal, drain_stderr, reader_task, writer_task};
use crate::protocol::{
    self, CallToolResult, Incoming, RequestId, RpcError, RpcNotification, RpcRequest, ToolDef,
    method,
};

/// The hard cap on one stdio stdout line: a server that cannot frame
/// is a server that cannot be trusted with this process's memory.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The stderr ring size kept for a stdio server's death report.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// The hard cap on one HTTP response body, or one SSE event's data:
/// the HTTP transport's counterpart to [`MAX_LINE_BYTES`]. A server
/// that sends more has stopped framing.
pub const MAX_HTTP_BODY_BYTES: usize = MAX_LINE_BYTES;

/// The startup budget [`Client::connect_http`] uses when the caller
/// names none.
pub const DEFAULT_HTTP_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

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
        // the child is already going. An unbounded sender lets the
        // handshake path abort the child too, on a failed or timed-out
        // handshake.
        let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<KillSignal>();
        let reader_shared = Arc::clone(&shared);
        tokio::spawn(reader_task(
            tokio::io::BufReader::new(stdout),
            reader_shared,
            kill_tx.clone(),
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
                signal = kill_rx.recv() => match signal {
                    Some(KillSignal::Violation) => {
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
                    Some(KillSignal::StreamEnded) => {
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
                    Some(KillSignal::Abort) => {
                        let _ = child_for_wait.start_kill();
                        let _ = child_for_wait.wait().await;
                    }
                    None => {}
                },
            }
        });

        let client = Self {
            shared,
            transport: Transport::Stdio,
            writer: Some(writer),
            http: None,
        };
        if let Err(err) = client.handshake(startup_timeout).await {
            // The client drops here, closing stdin. A server that
            // ignores EOF would otherwise outlive the failed attempt;
            // tell the watcher to kill and reap it.
            let _ = kill_tx.send(KillSignal::Abort);
            return Err(err);
        }
        Ok(client)
    }

    /// Connects over Streamable HTTP with the default startup budget.
    ///
    /// # Errors
    ///
    /// See [`Client::connect_http_with_timeout`].
    pub async fn connect_http(params: HttpParams) -> Result<Self, ClientError> {
        Self::connect_http_with_timeout(params, DEFAULT_HTTP_STARTUP_TIMEOUT).await
    }

    /// Connects over Streamable HTTP: performs the handshake POSTs
    /// and stores the session, all inside `startup_timeout`.
    ///
    /// # Errors
    ///
    /// URL parse failure, transport failure, a failed handshake
    /// exchange, an unsupported revision, or a handshake that outlived
    /// `startup_timeout`.
    pub async fn connect_http_with_timeout(
        params: HttpParams,
        startup_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let url: hyper::Uri = params
            .url
            .parse()
            .map_err(|err| ClientError::Http(format!("endpoint url: {err}")))?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client: HttpClient =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector);
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
        match tokio::time::timeout(startup_timeout, me.handshake_http()).await {
            Ok(result) => result?,
            Err(_elapsed) => return Err(ClientError::Timeout),
        }
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
                // The exchange runs its own clock, started once it
                // holds the session lock: the queue wait behind a
                // slower caller is not this call's budget.
                let fut = self.http_exchange(http, id, &line, timeout);
                tokio::pin!(fut);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.abandon_http(id).await;
                        Err(ClientError::Cancelled)
                    }
                    answer = &mut fut => match answer {
                        Err(err) if matches!(err, ClientError::Timeout) => {
                            self.abandon_http(id).await;
                            Err(err)
                        }
                        other => other,
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
}

/// Builds the `notifications/cancelled` wire line for a request id.
fn cancelled_line(id: u64) -> String {
    let params = serde_json::to_value(protocol::CancelledParams {
        request_id: RequestId::Number(id),
    })
    .expect("cancelled params serialize");
    RpcNotification::new(method::CANCELLED, Some(params)).to_line()
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
