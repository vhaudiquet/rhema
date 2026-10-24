//! The daemon half of the protocol: `rhema-sandboxd` served from a host.
//!
//! One [`FsRoot`] per session name under `--base`, created on first
//! use; exec through a [`LocalSandbox`] per request (its `timeout`
//! prefix is the kill, its stream pump the cap); files and seeds
//! straight on the root. The token is checked on every route,
//! `/healthz` included. The daemon adds no caps of its own: the
//! streams arrive tail-capped because [`LocalSandbox`] caps them
//! before the reply is built — the spec's "sandbox side caps streams"
//! clause, inherited rather than restated.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use clap::Parser;

use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::protocol::{
    Ack, EXEC, ExecCall, ExecReply, FileReply, GET, PUT, PutFile, SEED, SeedRequest, TOKEN_HEADER,
};
use rhema_sandbox::{ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
use rhema_sandbox_local::LocalSandbox;

/// `rhema-sandboxd`'s arguments: where sessions live, the shared
/// secret, and the address to serve on.
#[derive(Debug, Parser)]
#[command(
    name = "rhema-sandboxd",
    version,
    about = "The sandbox protocol served from a host"
)]
pub struct Args {
    /// The root directory every session lives under, one subdirectory
    /// per session name.
    #[arg(long)]
    pub base: PathBuf,

    /// The shared secret; a wrong or missing one is a 401 on every
    /// route, `/healthz` included.
    #[arg(long)]
    pub token: String,

    /// The address to bind; loopback unless the daemon is meant to be
    /// reached from elsewhere.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// The port to bind; `0` lets the OS pick, and the bound address
    /// is printed on startup so scripts can wait for it.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
}

/// The daemon state: the base directory, the shared secret, and the
/// one root per session name.
pub struct Daemon {
    /// The directory every session root is created under; the session
    /// name is validated to a single component before the join, so
    /// nothing climbs out of here.
    base: PathBuf,
    /// The shared secret checked on every route.
    token: String,
    /// The sessions seen so far; the first request for a name stands
    /// its root up. A plain mutex, not a concurrent map: the lock is
    /// never held across an await.
    sessions: Mutex<BTreeMap<String, Arc<FsRoot>>>,
}

impl Daemon {
    /// A daemon serving `base` with `token`; no session exists yet.
    pub fn new(base: PathBuf, token: String) -> Self {
        Self {
            base,
            token,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    /// The session's root, created under the base on first use. The
    /// first request for a name pays the `create_dir_all` and the
    /// canonicalize; every later one reuses the same root, so a
    /// session's files stay put across requests.
    fn session_root(&self, session: &str) -> Result<Arc<FsRoot>, SandboxError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(root) = sessions.get(session) {
            return Ok(Arc::clone(root));
        }
        let root = Arc::new(FsRoot::new(self.base.join(session))?);
        sessions.insert(session.to_owned(), Arc::clone(&root));
        Ok(root)
    }
}

/// The session path segment as a single safe component: no emptiness,
/// no traversal, no separator. The name becomes a directory under the
/// base, and the segments are remote-influenced — the same guard
/// [`FsRoot`] keeps inside the workspace, restated one level up, where
/// the daemon decides where a session's root even sits.
fn session_name(session: &str) -> Result<(), SandboxError> {
    let safe = !session.is_empty()
        && session != "."
        && session != ".."
        && !session.contains('/')
        && !session.contains('\\')
        && !session.contains('\u{0}');
    if safe {
        Ok(())
    } else {
        Err(SandboxError::Failed(format!(
            "the session name must be one safe path component: {session:?}"
        )))
    }
}

/// The daemon's routes: `/healthz` and the four protocol routes, one
/// session segment ahead of each — the paths are [`rhema_sandbox::protocol`]'s
/// constants, the token middleware layered over the lot.
pub fn router(daemon: Arc<Daemon>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(&format!("/{{session}}{EXEC}"), post(exec))
        .route(&format!("/{{session}}{PUT}"), post(put))
        .route(&format!("/{{session}}{GET}"), post(get_file))
        .route(&format!("/{{session}}{SEED}"), post(seed))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&daemon),
            require_token,
        ))
        .with_state(daemon)
}

/// Binds `args.host:args.port`, prints the bound address (the OS picks
/// one when `--port` is `0`), and serves until killed.
pub async fn run(args: Args) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let addr = listener.local_addr()?;
    let daemon = Arc::new(Daemon::new(args.base, args.token));
    println!("rhema-sandboxd listening on {addr}");
    axum::serve(listener, router(daemon)).await
}

/// The shared secret on every route: a wrong or missing token is a 401
/// before the route runs.
async fn require_token(
    State(daemon): State<Arc<Daemon>>,
    request: Request,
    next: Next,
) -> Response {
    let good = request
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|candidate| candidate == daemon.token);
    if !good {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "forbidden" })),
        )
            .into_response();
    }
    next.run(request).await
}

/// `GET /healthz`: the shape clients probe.
async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

/// `POST /{session}/exec`: one command through a fresh
/// [`LocalSandbox`] over the session's root.
async fn exec(
    Path(session): Path<String>,
    State(daemon): State<Arc<Daemon>>,
    Json(call): Json<ExecCall>,
) -> Response {
    if let Err(err) = session_name(&session) {
        return error_response(err);
    }
    let root = match daemon.session_root(&session) {
        Ok(root) => root,
        Err(err) => return error_response(err),
    };
    let sandbox = LocalSandbox::new((*root).clone());
    let request = ExecRequest {
        command: call.command,
        cwd: call.cwd.unwrap_or_default(),
        timeout: Duration::from_secs(call.timeout_secs),
    };
    // No streaming here: the reply carries the completed output, and
    // the caps it arrives under are LocalSandbox's, applied before
    // this builds the reply.
    match sandbox
        .exec(request, &AtomicBool::new(false), &mut |_| {})
        .await
    {
        Ok(outcome) => Json(ExecReply {
            exit_code: outcome.code,
            stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
            timed_out: false,
        })
        .into_response(),
        // A timeout crosses as the reply's `timed_out`, never as an
        // error status: the client maps that field to its timeout
        // error, and an error status would surface as unavailability.
        // 137 is the kill code the protocol's shape test pins.
        Err(SandboxError::Timeout(_)) => Json(ExecReply {
            exit_code: Some(137),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        })
        .into_response(),
        Err(err) => error_response(err),
    }
}

/// `POST /{session}/put`: one file straight onto the session's root.
async fn put(
    Path(session): Path<String>,
    State(daemon): State<Arc<Daemon>>,
    Json(put): Json<PutFile>,
) -> Response {
    match write_file(&daemon, &session, &put).await {
        Ok(ack) => Json(ack).into_response(),
        Err(err) => error_response(err),
    }
}

async fn write_file(daemon: &Daemon, session: &str, put: &PutFile) -> Result<Ack, SandboxError> {
    session_name(session)?;
    let mapped = daemon.session_root(session)?.map(&put.path)?;
    let contents = base64::engine::general_purpose::STANDARD
        .decode(&put.content_base64)
        .map_err(|err| SandboxError::Failed(format!("the put body is not base64: {err}")))?;
    // The shim's put creates the parent directories, and the skills
    // seeding writes two levels deep — the local route keeps that
    // contract.
    if let Some(parent) = mapped.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| SandboxError::Failed(format!("{}: {err}", put.path)))?;
    }
    tokio::fs::write(&mapped, contents)
        .await
        .map_err(|err| SandboxError::Failed(format!("{}: {err}", put.path)))?;
    Ok(Ack { error: None })
}

/// The request body of [`GET`]: `{ "path": ... }` — the one shape
/// `protocol.rs` pins in prose only, so the daemon keeps a private
/// type for it rather than parsing loose JSON.
#[derive(Debug, serde::Deserialize)]
struct GetFile {
    /// The sandbox-absolute POSIX path to read.
    path: String,
}

/// `POST /{session}/get`: one file read straight off the session's
/// root; a missing file is a 404 carrying the error.
async fn get_file(
    Path(session): Path<String>,
    State(daemon): State<Arc<Daemon>>,
    Json(body): Json<GetFile>,
) -> Response {
    if let Err(err) = session_name(&session) {
        return error_response(err);
    }
    let mapped = match daemon
        .session_root(&session)
        .and_then(|r| r.map(&body.path))
    {
        Ok(mapped) => mapped,
        Err(err) => return error_response(err),
    };
    match tokio::fs::read(&mapped).await {
        Ok(contents) => Json(FileReply {
            content_base64: Some(base64::engine::general_purpose::STANDARD.encode(contents)),
            error: None,
        })
        .into_response(),
        Err(err) => {
            let status = if err.kind() == std::io::ErrorKind::NotFound {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                status,
                Json(FileReply {
                    content_base64: None,
                    error: Some(format!("{}: {err}", body.path)),
                }),
            )
                .into_response()
        }
    }
}

/// `POST /{session}/seed`: the [`WorkspaceSeed`] ingress, through
/// [`LocalSandbox::seed`] — the clone and the unpack run under the
/// same budget and caps as every other command.
async fn seed(
    Path(session): Path<String>,
    State(daemon): State<Arc<Daemon>>,
    Json(request): Json<SeedRequest>,
) -> Response {
    if let Err(err) = session_name(&session) {
        return error_response(err);
    }
    let root = match daemon.session_root(&session) {
        Ok(root) => root,
        Err(err) => return error_response(err),
    };
    let seed = match request {
        SeedRequest::Git { git } => WorkspaceSeed::GitClone(git),
        // The wire's name is diagnostics only; the unpack is at the
        // root whatever it says.
        SeedRequest::Tarball {
            tarball_base64,
            tarball_name,
        } => WorkspaceSeed::Tarball {
            name: tarball_name.unwrap_or_default(),
            tar_gz_base64: tarball_base64,
        },
    };
    match LocalSandbox::new((*root).clone()).seed(&seed).await {
        Ok(()) => Json(Ack { error: None }).into_response(),
        Err(err) => error_response(err),
    }
}

/// One handler failure as the protocol's error convention: the status
/// carries the class (4xx the caller's input, 5xx the daemon's own
/// trouble), the body carries the message.
fn error_response(err: SandboxError) -> Response {
    let status = match &err {
        SandboxError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        SandboxError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        SandboxError::Failed(_) => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_must_be_single_safe_components() {
        for good in ["sess-7", "workspace_one", "a", "Up1"] {
            assert!(session_name(good).is_ok(), "{good:?} is a safe name");
        }
        for bad in ["", ".", "..", "a/b", "../etc", "a\\b", "a\u{0}b", "/"] {
            let error = session_name(bad).expect_err("the name is refused");
            assert!(
                matches!(error, SandboxError::Failed(_)),
                "the refusal is in band: {error:?}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod loopback_tests {
    //! The loopback contract (spec §6): the daemon in-process on an
    //! ephemeral port, driven by the real client — the two sides
    //! exercised together, not asserted at each other.

    use super::*;
    use rhema_sandbox::ExecOutcome;
    use rhema_sandbox_http::HttpSandbox;

    /// The daemon's token in these tests.
    const TOKEN: &str = "sekrit";

    /// The daemon on an ephemeral loopback port over a fresh base; the
    /// guard comes back so the base outlives the test.
    async fn spawn_daemon() -> (String, tempfile::TempDir) {
        let base = tempfile::tempdir().expect("the base is made");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback port binds");
        let addr = listener.local_addr().expect("the address reads");
        let daemon = Arc::new(Daemon::new(base.path().to_path_buf(), TOKEN.to_owned()));
        tokio::spawn(async move {
            axum::serve(listener, router(daemon))
                .await
                .expect("the server serves");
        });
        (format!("http://{addr}"), base)
    }

    /// A client for session `sess-7` holding the right token.
    fn sandbox(base: &str) -> HttpSandbox {
        HttpSandbox::new(base.to_owned(), "sess-7", TOKEN)
    }

    /// One command at the root, budgeted by the caller.
    fn exec_request(command: &str, timeout: Duration) -> ExecRequest {
        ExecRequest {
            command: command.to_owned(),
            cwd: "/".into(),
            timeout,
        }
    }

    /// One exec through the client with nothing cancelled and nothing
    /// watching the progress — the shape most tests want.
    async fn run(sandbox: &HttpSandbox, request: ExecRequest) -> Result<ExecOutcome, SandboxError> {
        let cancelled = AtomicBool::new(false);
        sandbox.exec(request, &cancelled, &mut |_| {}).await
    }

    /// The healthz route probed directly, with or without a token; the
    /// status and the decoded body come back.
    async fn probe_healthz(base: &str, token: Option<&str>) -> (u16, serde_json::Value) {
        let http = reqwest::Client::new();
        let mut request = http.get(format!("{base}/healthz"));
        if let Some(token) = token {
            request = request.header(TOKEN_HEADER, token);
        }
        let response = request.send().await.expect("the probe answers");
        let status = response.status().as_u16();
        (status, response.json().await.expect("the body parses"))
    }

    /// Runs one git command in `dir`; the global and system config are
    /// blanked so the fixture does not depend on the host's.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_reports_success_nonzero_and_the_timeout_kill() {
        let (base, _dir) = spawn_daemon().await;
        let sbx = sandbox(&base);

        // Success: the stdout and the exit code cross as sent.
        let outcome = run(&sbx, exec_request("echo hi", Duration::from_secs(30)))
            .await
            .expect("the command runs");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");

        // Nonzero: the real code crosses.
        let outcome = run(&sbx, exec_request("exit 3", Duration::from_secs(30)))
            .await
            .expect("the command runs");
        assert_eq!(outcome.code, Some(3));

        // Timeout: the daemon's `timeout` prefix kills at the budget,
        // the reply's `timed_out` crosses, and the client maps it to
        // its own timeout error.
        let error = run(&sbx, exec_request("sleep 5", Duration::from_secs(1)))
            .await
            .expect_err("the budget kills the command");
        assert_eq!(error, SandboxError::Timeout(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_then_get_round_trips_and_a_missing_get_fails() {
        let (base, _dir) = spawn_daemon().await;
        let sbx = sandbox(&base);

        sbx.write_file("/workspace/deep/note.md", b"# note")
            .await
            .expect("the write lands");
        let read = sbx
            .read_file("/workspace/deep/note.md")
            .await
            .expect("the read lands");
        assert_eq!(read, b"# note");

        // A missing file is a 404 from the daemon; the client reads
        // every non-2xx as unavailability carrying the status.
        let error = sbx
            .read_file("/workspace/absent")
            .await
            .expect_err("the read fails");
        match &error {
            SandboxError::Unavailable(message) => {
                assert!(message.contains("404"), "the miss is a 404: {message}");
            }
            other => panic!("a missing read fails in band, not {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seeding_lands_from_git_and_from_a_tarball() {
        let (base, _dir) = spawn_daemon().await;

        // Git: a one-commit repository behind a file:// URL.
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("seeded.txt"), b"from git")
            .expect("the fixture file is written");
        git(repo.path(), &["add", "."]);
        git(
            repo.path(),
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        let git_session = HttpSandbox::new(base.clone(), "git-seed", TOKEN);
        git_session
            .seed(&WorkspaceSeed::GitClone(format!(
                "file://{}",
                repo.path().display()
            )))
            .await
            .expect("the clone lands");
        let outcome = run(&git_session, exec_request("ls", Duration::from_secs(30)))
            .await
            .expect("the listing runs");
        let listing = String::from_utf8(outcome.stdout).expect("stdout is text");
        assert!(
            listing.contains("seeded.txt"),
            "the clone landed: {listing}"
        );

        // Tarball: one file, packed and base64'd, unpacked at the root.
        let payload = tempfile::tempdir().expect("tempdir");
        std::fs::write(payload.path().join("packed.txt"), b"from tar")
            .expect("the payload file is written");
        let archive = payload.path().join("packed.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(payload.path())
            .arg("packed.txt")
            .status()
            .expect("tar runs");
        assert!(status.success(), "the fixture tarball is built");
        let tar_session = HttpSandbox::new(base.clone(), "tar-seed", TOKEN);
        tar_session
            .seed(&WorkspaceSeed::Tarball {
                name: "packed.tar.gz".into(),
                tar_gz_base64: base64::engine::general_purpose::STANDARD
                    .encode(std::fs::read(&archive).expect("the tarball is read")),
            })
            .await
            .expect("the unpack lands");
        let outcome = run(&tar_session, exec_request("ls", Duration::from_secs(30)))
            .await
            .expect("the listing runs");
        let listing = String::from_utf8(outcome.stdout).expect("stdout is text");
        assert!(
            listing.contains("packed.txt"),
            "the unpack landed: {listing}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_token_is_refused_on_exec_and_healthz() {
        let (base, _dir) = spawn_daemon().await;

        // Exec: the real client with the wrong token — a non-2xx reads
        // as unavailability carrying the status.
        let intruder = HttpSandbox::new(base.clone(), "sess-7", "wrong");
        let error = run(&intruder, exec_request("true", Duration::from_secs(30)))
            .await
            .expect_err("the refusal surfaces");
        match &error {
            SandboxError::Unavailable(message) => {
                assert!(message.contains("401"), "the status crosses: {message}");
            }
            other => panic!("a refused token is unavailable, not {other:?}"),
        }

        // healthz: wrong and missing are both a 401 with the pinned
        // body; the right token is answered.
        let (status, body) = probe_healthz(&base, Some("wrong")).await;
        assert_eq!(status, 401);
        assert_eq!(body, serde_json::json!({ "error": "forbidden" }));
        let (status, body) = probe_healthz(&base, None).await;
        assert_eq!(status, 401);
        assert_eq!(body, serde_json::json!({ "error": "forbidden" }));
        let (status, body) = probe_healthz(&base, Some(TOKEN)).await;
        assert_eq!(status, 200);
        assert_eq!(body, serde_json::json!({ "ok": true }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stream_cap_keeps_the_tail() {
        let (base, _dir) = spawn_daemon().await;
        let sbx = sandbox(&base);

        // 200 KB of stdout crosses capped: at most the shim's 50 KiB,
        // because LocalSandbox caps before the daemon builds the reply.
        let outcome = run(
            &sbx,
            exec_request("yes | head -c 200000", Duration::from_secs(30)),
        )
        .await
        .expect("the command runs");
        assert!(
            outcome.stdout.len() <= 50 * 1024,
            "the stream is capped: {}",
            outcome.stdout.len()
        );

        // The tail is kept, and distinct lines make it visible: the
        // last line of `seq`'s output, not the first.
        let outcome = run(&sbx, exec_request("seq 1 200000", Duration::from_secs(30)))
            .await
            .expect("the command runs");
        assert!(outcome.stdout.len() <= 50 * 1024);
        assert!(
            outcome.stdout.ends_with(b"200000\n"),
            "the last bytes survive the cap: {:?}",
            String::from_utf8_lossy(&outcome.stdout),
        );
    }
}
