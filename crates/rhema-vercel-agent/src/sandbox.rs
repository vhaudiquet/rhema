//! `bash` in a Vercel Sandbox: the [`Sandbox`] seam over the REST
//! client.
//!
//! A named sandbox per session id, resumed or created on first use —
//! persistence is the platform's default, so the workspace survives
//! idle periods the way the Cloudflare container does across sleeps.
//! Commands run as `bash -c` (the REST API wants a program plus
//! arguments; the shell tool's commands are shell lines), with the
//! request's budget handed to the sandbox as the kill timeout and
//! restated as this side's poll deadline. Output is polled, not
//! streamed: the seam's callback shape is honored by delivering the
//! complete lines at completion, which the contract allows v1
//! transports to do.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rhema_cloud_agent::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
use tokio::sync::Mutex;

use crate::api::{ApiError, CmdRequest, SandboxApi};

/// How often a running command's status is polled. The platform's
/// kill timeout bounds the command; this bounds only the wait.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// The tail cap per stream, in bytes — the seam's contract says the
/// sandbox side caps before the wire; the REST logs arrive whole, so
/// the cap lands here.
const STREAM_CAP: usize = 128 * 1024;

/// The resolved session, plus whether the workspace is known to
/// exist — the `mkdir -p` runs once per session, not once per
/// command.
#[derive(Default)]
struct Resolved {
    id: String,
    workspace_ready: bool,
}

/// A Vercel Sandbox bound to one session name. Cheap to clone; the
/// session resolves once, lazily, behind a shared cell.
pub struct VercelSandbox {
    api: Arc<dyn SandboxApi>,
    name: String,
    workspace: String,
    session: Mutex<Option<Resolved>>,
}

impl VercelSandbox {
    /// A sandbox for the session named `name`, commands rooted at
    /// `workspace`, calls riding `api`.
    pub fn new(api: Arc<dyn SandboxApi>, name: String, workspace: String) -> Self {
        Self {
            api,
            name,
            workspace,
            session: Mutex::new(None),
        }
    }

    /// The live session id, resuming or creating the named sandbox on
    /// first use. The platform's create is idempotent per name within
    /// a project: a resume that comes back `not found` means the
    /// sandbox was never made.
    /// The live session, resuming or creating the named sandbox on
    /// first use and making the workspace once. The platform's create
    /// is idempotent per name within a project: a resume that comes
    /// back `not found` means the sandbox was never made.
    async fn session_id(&self) -> Result<String, SandboxError> {
        let mut guard = self.session.lock().await;
        let resolved = match guard.as_mut() {
            Some(resolved) => resolved,
            None => {
                let id = match self.api.named_session(&self.name).await {
                    Ok(handle) => handle.inner.id,
                    Err(ApiError::NotFound(_)) => {
                        self.api
                            .create_named(&self.name)
                            .await
                            .map_err(api_error)?
                            .inner
                            .id
                    }
                    Err(err) => return Err(api_error(err)),
                };
                guard.insert(Resolved {
                    id,
                    workspace_ready: false,
                })
            }
        };
        // The workspace exists, even on a brand-new image: the
        // platform's home is not ours.
        if !resolved.workspace_ready {
            self.run(
                &resolved.id.clone(),
                &format!("mkdir -p '{}'", self.workspace),
                Duration::from_secs(30),
            )
            .await?;
            resolved.workspace_ready = true;
        }
        Ok(resolved.id.clone())
    }

    /// One command, polled to completion: the logs' two streams and
    /// the exit code.
    async fn run(
        &self,
        session: &str,
        command: &str,
        budget: Duration,
    ) -> Result<(Vec<u8>, Vec<u8>, Option<i32>), SandboxError> {
        let started = self
            .api
            .exec(
                session,
                &CmdRequest {
                    command: "bash".into(),
                    args: vec!["-c".into(), command.to_owned()],
                    cwd: self.workspace.clone(),
                    timeout: budget.as_millis() as u64,
                },
            )
            .await
            .map_err(api_error)?;
        let cmd = started.inner.id;
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let status = self.api.command(session, &cmd).await.map_err(api_error)?;
            if let Some(code) = status.inner.exit_code {
                break self.collect(session, &cmd, code).await;
            }
            if tokio::time::Instant::now() >= deadline {
                // The sandbox kills at its own timeout too; ours makes
                // the wait deterministic even if the clock skews.
                let _ = self.api.kill(session, &cmd).await;
                return Err(SandboxError::Timeout(budget.as_secs().max(1)));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// The finished command's output, split by stream and tail-capped.
    async fn collect(
        &self,
        session: &str,
        cmd: &str,
        code: i32,
    ) -> Result<(Vec<u8>, Vec<u8>, Option<i32>), SandboxError> {
        let entries = self.api.logs(session, cmd).await.map_err(api_error)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for entry in entries {
            match entry.stream.as_str() {
                "stdout" => cap(&mut stdout, entry.data),
                _ => cap(&mut stderr, entry.data),
            }
        }
        Ok((stdout, stderr, Some(code)))
    }
}

/// Appends `data`, keeping the tail past the cap.
fn cap(into: &mut Vec<u8>, data: String) {
    into.extend_from_slice(data.as_bytes());
    if into.len() > STREAM_CAP {
        let overflow = into.len() - STREAM_CAP;
        into.drain(..overflow);
    }
}

/// Every API error becomes the sandbox error the tool renders in band.
fn api_error(err: ApiError) -> SandboxError {
    match err {
        ApiError::NotFound(what) => SandboxError::Failed(format!("{what}: not found")),
        ApiError::Failed(message) => SandboxError::Unavailable(message),
    }
}

fn poll_cancelled(cancelled: &AtomicBool) -> bool {
    cancelled.load(Ordering::Relaxed)
}

#[async_trait]
impl Sandbox for VercelSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        if poll_cancelled(cancelled) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let session = self.session_id().await?;
        let (stdout, stderr, code) = self
            .run(&session, &request.command, request.timeout)
            .await?;
        // Complete lines, as the v1 contract allows; the callback
        // still sees every line the command completed.
        for line in String::from_utf8_lossy(&stdout).lines() {
            if poll_cancelled(cancelled) {
                return Err(SandboxError::Failed("cancelled".into()));
            }
            progress(line);
        }
        Ok(ExecOutcome {
            stdout,
            stderr,
            code,
        })
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        let session = self.session_id().await?;
        match seed {
            WorkspaceSeed::GitClone(url) => self
                .run(
                    &session,
                    &format!("git clone --depth 1 '{url}' ."),
                    Duration::from_secs(300),
                )
                .await
                .map(|_| ()),
            WorkspaceSeed::Tarball {
                name: _,
                tar_gz_base64,
            } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64.as_bytes())
                    .map_err(|err| SandboxError::Failed(format!("unreadable tarball: {err}")))?;
                self.api
                    .write_files(&session, &self.workspace, bytes)
                    .await
                    .map_err(api_error)
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        let session = self.session_id().await?;
        match self.api.read_file(&session, path).await {
            Ok(bytes) => Ok(bytes),
            Err(ApiError::NotFound(_)) => {
                Err(SandboxError::Failed(format!("{path}: no such file")))
            }
            Err(err) => Err(api_error(err)),
        }
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        let session = self.session_id().await?;
        let tar_gz = tarball(path, contents)?;
        self.api
            .write_files(&session, "/", tar_gz)
            .await
            .map_err(api_error)
    }
}

/// Builds the one-file gzipped tarball the fs/write endpoint wants.
/// `path` is absolute inside the sandbox; the tarball carries it
/// relative, and `x-cwd: /` puts it back.
fn tarball(path: &str, contents: &[u8]) -> Result<Vec<u8>, SandboxError> {
    let relative = path.trim_start_matches('/');
    if relative.is_empty() {
        return Err(SandboxError::Failed("write: empty path".into()));
    }
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, relative, contents)
        .map_err(|err| SandboxError::Failed(format!("tarball: {err}")))?;
    builder
        .into_inner()
        .map_err(|err| SandboxError::Failed(format!("tarball: {err}")))?
        .finish()
        .map_err(|err| SandboxError::Failed(format!("tarball: {err}")))
}
