//! The sandbox client: the [`Sandbox`] seam over the session's
//! SandboxShim Durable Object.
//!
//! The shim (a small JS DO, `deploy/cloudflare/sandbox-shim.mjs`) owns
//! the container binding — the container runtime is a JS-only API — and
//! speaks plain JSON over its DO fetch: exec, seed, get, put. The two
//! DOs of one session share a name, so binding by the session's id
//! reaches the container whose lifetime matches the session's
//! checkpoints. A shared secret guards the shim's HTTP surface.
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use base64::Engine;
use worker::{Headers, Method, ObjectNamespace, Request, RequestInit};

use rhema_cloud_agent::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// The sandbox of one session, over its DO binding.
pub struct SandboxDo {
    namespace: ObjectNamespace,
    /// The DO name — both DOs of a session derive from the same string.
    name: String,
    /// The shared secret the shim checks.
    token: String,
}

impl SandboxDo {
    /// A client for the sandbox DO named `name` in `namespace`.
    pub fn new(
        namespace: ObjectNamespace,
        name: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            namespace,
            name: name.into(),
            token: token.into(),
        }
    }

    /// One JSON POST to the shim: the whole exchange — fetch, status,
    /// body text — runs inside one local task, because every piece is
    /// JS-backed and `!Send`; what crosses back is plain data.
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        body: &serde_json::Value,
    ) -> Result<T, SandboxError> {
        let id = self
            .namespace
            .id_from_name(&self.name)
            .map_err(shim_error)?;
        let stub = id.get_stub().map_err(shim_error)?;
        let mut init = RequestInit::new();
        init.method = Method::Post;
        init.headers = Headers::new();
        init.headers
            .set("x-rhema-token", &self.token)
            .map_err(shim_error)?;
        init.headers
            .set("content-type", "application/json")
            .map_err(shim_error)?;
        let payload = serde_json::to_vec(body).map_err(|err| {
            SandboxError::Unavailable(format!("request does not serialize: {err}"))
        })?;
        init.body = Some(worker::js_sys::Uint8Array::from(payload.as_slice()).into());
        let request = Request::new_with_init(&format!("https://sandbox.shim{route}"), &init)
            .map_err(shim_error)?;
        let (status_slot, status) = futures::channel::oneshot::channel();
        let (body_slot, body) = futures::channel::oneshot::channel::<String>();
        wasm_bindgen_futures::spawn_local(async move {
            let outcome: worker::Result<(u16, String)> = async {
                let mut response = stub.fetch_with_request(request).await?;
                let status = response.status_code();
                let text = response.text().await?;
                worker::Result::Ok((status, text))
            }
            .await;
            match outcome {
                Ok((status, text)) => {
                    let _ = status_slot.send(status);
                    let _ = body_slot.send(text);
                }
                Err(err) => {
                    let _ = status_slot.send(0);
                    let _ = body_slot.send(err.to_string());
                }
            }
        });
        let status = status
            .await
            .map_err(|_| shim_error(worker::Error::from("the shim call ended without a status")))?;
        let text = match body.await {
            Ok(text) => text,
            Err(_) => {
                return Err(shim_error(worker::Error::from(
                    "the shim call ended without a body",
                )));
            }
        };
        if !(200..300).contains(&status) {
            return Err(SandboxError::Unavailable(format!(
                "the sandbox shim answered (HTTP {status}): {text}"
            )));
        }
        serde_json::from_str(&text)
            .map_err(|err| SandboxError::Unavailable(format!("malformed shim reply: {err}")))
    }
}

/// Maps a transport failure below the seam: unreachable, not failed.
fn shim_error(err: worker::Error) -> SandboxError {
    SandboxError::Unavailable(err.to_string())
}

/// The shim's exec reply.
#[derive(serde::Deserialize)]
struct ShimExec {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    /// Whether the shim's `timeout` killed the command.
    timed_out: bool,
}

/// Streams the shim's (already tail-capped) output through `progress`:
/// stdout's lines first, then stderr's. The v1 shim buffers the whole
/// output, so this is one burst, not live lines — the callback shape is
/// the contract a streaming shim fills in.
fn stream_outcome(outcome: &ShimExec, progress: &mut (dyn for<'a> FnMut(&'a str) + Send)) {
    for line in outcome.stdout.lines() {
        progress(&format!("{line}\n"));
    }
    for line in outcome.stderr.lines() {
        progress(&format!("{line}\n"));
    }
}

#[async_trait]
impl Sandbox for SandboxDo {
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let reply: ShimExec = self
            .post(
                "/exec",
                &serde_json::json!({
                    "command": request.command,
                    "cwd": request.cwd,
                    "timeout_secs": request.timeout.as_secs(),
                }),
            )
            .await?;
        // No cancel check here: the command completed remotely and its
        // result is real — the model sees the truth, whatever the turn
        // decided meanwhile.
        if reply.timed_out {
            return Err(SandboxError::Timeout(request.timeout.as_secs()));
        }
        stream_outcome(&reply, progress);
        Ok(ExecOutcome {
            stdout: reply.stdout.into_bytes(),
            stderr: reply.stderr.into_bytes(),
            code: reply.exit_code,
        })
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        let body = match seed {
            WorkspaceSeed::GitClone(url) => serde_json::json!({ "git": url }),
            WorkspaceSeed::Tarball {
                name,
                tar_gz_base64,
            } => {
                serde_json::json!({ "tarball_name": name, "tarball_base64": tar_gz_base64 })
            }
        };
        let reply: ShimAck = self.post("/seed", &body).await?;
        reply.into_result()
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        let reply: ShimFile = self
            .post("/get", &serde_json::json!({ "path": path }))
            .await?;
        if let Some(error) = reply.error {
            return Err(SandboxError::Failed(error));
        }
        base64::engine::general_purpose::STANDARD
            .decode(reply.content_base64.as_deref().unwrap_or_default())
            .map_err(|err| SandboxError::Failed(format!("malformed file reply: {err}")))
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(contents);
        let reply: ShimAck = self
            .post(
                "/put",
                &serde_json::json!({ "path": path, "content_base64": encoded }),
            )
            .await?;
        reply.into_result()
    }
}

/// The shim's ack: `error` null on success, the failure message
/// otherwise.
#[derive(serde::Deserialize)]
struct ShimAck {
    #[serde(default)]
    error: Option<String>,
}

impl ShimAck {
    /// The ack as a sandbox result.
    fn into_result(self) -> Result<(), SandboxError> {
        match self.error {
            Some(message) => Err(SandboxError::Failed(message)),
            None => Ok(()),
        }
    }
}

/// The shim's file reply.
#[derive(serde::Deserialize)]
struct ShimFile {
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    error: Option<String>,
}
